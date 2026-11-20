use super::*;
use cratonvm_vm::config::{
    CompatibilityMode, JdkMode, EMBEDDED_DEFAULT_JDK_MODE, LAUNCHER_DEFAULT_JDK_MODE,
    SYNTHETIC_JDK_COMPILED_IN,
};

/// `launcher_jar_file_name` is `java.io.File.getPath()`'s normalisation
/// (interpreter round i2 wave 60, lane L5).
#[test]
fn launcher_jar_file_name_normalises_like_java_io_file() {
    if cfg!(windows) {
        assert_eq!(launcher_jar_file_name("out/nomf.jar"), "out\\nomf.jar");
        assert_eq!(
            launcher_jar_file_name("out//x\\\\nomf.jar"),
            "out\\x\\nomf.jar"
        );
        assert_eq!(
            launcher_jar_file_name("\\\\host\\share\\a.jar"),
            "\\\\host\\share\\a.jar"
        );
        assert_eq!(launcher_jar_file_name("C:/"), "C:\\");
    } else {
        assert_eq!(launcher_jar_file_name("out/nomf.jar"), "out/nomf.jar");
        assert_eq!(launcher_jar_file_name("out//x/nomf.jar/"), "out/x/nomf.jar");
        assert_eq!(launcher_jar_file_name("/"), "/");
    }
}

// -----------------------------------------------------------------------
// JDK-mode determinism (2026-07-26)
//
// The launcher used to pick its standard library by sniffing the host
// (`use_synthetic_jdk = detect_real_jdk().is_none()`). These tests pin
// the replacement contract: fixed default, symmetric explicit flags,
// no silent fallback, and a mode that is visible in `-version`.
// -----------------------------------------------------------------------

fn tokens(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

// -----------------------------------------------------------------------
// The WARN floor and `RUST_LOG`.
//
// `run()` adds a targetless WARN directive so a default run still shows
// warnings. `RUST_LOG=info` is targetless too, and `EnvFilter` breaks a
// same-target tie by last-added-wins — so adding the floor unconditionally
// silently downgraded an explicit global level back to WARN, and
// `RUST_LOG=info cratonvm <workload>` emitted ZERO info events. That is the
// protocol `jit/src/platform.rs`'s REVIEW-NOTE 3 tells a reader to run to
// read the JIT code-arena census off stderr.
// -----------------------------------------------------------------------

#[test]
fn an_explicit_global_rust_log_level_suppresses_the_warn_floor() {
    // The exact spelling REVIEW-NOTE 3 documents, and the reason this
    // function exists.
    assert!(rust_log_names_a_global_level(Some("info")));
    assert!(rust_log_names_a_global_level(Some("debug")));
    assert!(rust_log_names_a_global_level(Some("trace")));
    // Mixed: a global level plus a per-target override is still global.
    assert!(rust_log_names_a_global_level(Some("info,hyper=warn")));
    assert!(rust_log_names_a_global_level(Some("hyper=warn,info")));
    // Whitespace is `EnvFilter`'s to ignore, and ours.
    assert!(rust_log_names_a_global_level(Some(" info , hyper=warn ")));
}

#[test]
fn a_target_only_rust_log_keeps_the_warn_floor() {
    // These name no level for other targets, so the floor is what keeps a
    // warning from an unrelated crate visible. Dropping it here would make
    // `--print-gpu-decisions` (which adds exactly such a directive) hide
    // every warning in the process.
    assert!(!rust_log_names_a_global_level(Some("cratonvm_jit=info")));
    assert!(!rust_log_names_a_global_level(Some(
        "cratonvm_vm::runtime::offload=debug"
    )));
    assert!(!rust_log_names_a_global_level(Some(
        "cratonvm_jit=info,cratonvm_vm=warn"
    )));
    // Span/field syntax carries an `=` and is not a global level either.
    assert!(!rust_log_names_a_global_level(Some("[compile]=trace")));
}

#[test]
fn an_absent_or_empty_rust_log_keeps_the_warn_floor() {
    // The default run: unchanged behaviour is the whole requirement here.
    assert!(!rust_log_names_a_global_level(None));
    assert!(!rust_log_names_a_global_level(Some("")));
    assert!(!rust_log_names_a_global_level(Some("   ")));
    // `EnvFilter` ignores empty directives; so must this, or `RUST_LOG=,`
    // would read as a global level and silence every warning.
    assert!(!rust_log_names_a_global_level(Some(",,")));
    assert!(!rust_log_names_a_global_level(Some("cratonvm_jit=info,")));
}

#[test]
fn launcher_default_mode_is_real_jdk() {
    assert_eq!(LAUNCHER_DEFAULT_JDK_MODE, JdkMode::Real);
    assert_eq!(
        scan_requested_jdk_mode(&tokens(&["cratonvm", "Main", "--"])),
        JdkMode::Real
    );
    // ...and the embedding default deliberately differs.
    assert_eq!(EMBEDDED_DEFAULT_JDK_MODE, JdkMode::Synthetic);
}

#[test]
fn explicit_mode_flags_are_symmetric() {
    assert_eq!(
        scan_requested_jdk_mode(&tokens(&["cratonvm", "--synthetic-jdk", "Main", "--"])),
        JdkMode::Synthetic
    );
    assert_eq!(
        scan_requested_jdk_mode(&tokens(&["cratonvm", "--real-jdk", "Main", "--"])),
        JdkMode::Real
    );
}

/// A program argument after `--` must never be mistaken for a launcher
/// mode flag.
#[test]
fn mode_flags_after_separator_are_program_args() {
    assert_eq!(
        scan_requested_jdk_mode(&tokens(&["cratonvm", "Main", "--", "--synthetic-jdk"])),
        JdkMode::Real
    );
}

#[test]
fn both_mode_flags_is_an_error_not_a_silent_winner() {
    let err = resolve_jdk_mode(true, true, None).expect_err("both flags must be rejected");
    let msg = format!("{err:#}");
    assert!(msg.contains("--synthetic-jdk"), "{msg}");
    assert!(msg.contains("--real-jdk"), "{msg}");
    assert!(msg.contains("standard-library implementations"), "{msg}");
}

/// Selecting synthetic mode in a build without the `synthetic-jdk`
/// Cargo feature must abort the launch. Booting anyway would register
/// none of the ~5,200 stubs *and* skip boot-classpath discovery,
/// producing a VM with no class library at all.
#[test]
fn synthetic_mode_requires_the_cargo_feature() {
    let result = resolve_jdk_mode(true, false, None);
    assert_eq!(result.is_ok(), SYNTHETIC_JDK_COMPILED_IN);
    if let Err(e) = result {
        let msg = format!("{e:#}");
        assert!(msg.contains("synthetic-jdk"), "{msg}");
    }
}

/// The general-bugs TODO's "update the usage docs accordingly": the
/// `synthetic-jdk` build requirement is a property a user hits at launch,
/// so `--help` has to state it. The rejection message alone is not
/// documentation — it only appears after the run has already failed.
#[test]
fn the_usage_text_states_the_synthetic_jdk_build_requirement() {
    // `LONG_ABOUT` is hand-wrapped to the help column, so a phrase can be
    // split across lines with the next line's indent in between. Collapse
    // whitespace before matching rather than pinning today's line breaks.
    let flat = LONG_ABOUT.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("synthetic-jdk` Cargo feature"),
        "--cratonvm-help must name the Cargo feature --synthetic-jdk needs"
    );
    assert!(
        flat.contains("jdk.mode.synthetic_compiled_in"),
        "--cratonvm-help must point at the way to check whether THIS build has it"
    );
    // zt t18/tg8: this text is `--cratonvm-help`'s since the HotSpot help
    // options print HotSpot's texts; it says so, and names the kill.
    assert!(flat.contains("--cratonvm-help prints this text"));
    assert!(flat.contains("CRATONVM_LAUNCHER_HOTSPOT_HELP_TEXT=0"));
}

/// `--real-jdk` must select the real library outright, including in a
/// build that does have the synthetic one compiled in. The two flags are
/// symmetric selections, not a preference the build configuration can
/// override.
#[test]
fn real_jdk_flag_selects_real_mode_regardless_of_the_synthetic_feature() {
    // Point at a nonexistent JAVA_HOME so the mode is decided without
    // depending on whether this machine has a JDK: an `Err` naming the
    // real-JDK search proves real mode was chosen, and an `Ok` carries the
    // mode directly.
    match resolve_jdk_mode(false, true, Some("/definitely/not/a/jdk/anywhere")) {
        Ok((mode, _)) => assert_eq!(mode, cratonvm_vm::config::JdkMode::Real),
        Err(e) => {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("no usable JDK was found"),
                "--real-jdk must fail through the REAL-JDK path, not fall \
                 back to synthetic: {msg}"
            );
        }
    }
}

/// Real-JDK mode with a bogus `--java-home` must fail loudly, naming
/// what was searched — never fall through to the synthetic library.
#[test]
fn real_mode_without_a_jdk_fails_loudly() {
    let err = resolve_jdk_mode(false, false, Some("/definitely/not/a/jdk/anywhere"))
        .expect_err("a nonexistent --java-home must not be tolerated");
    let msg = format!("{err:#}");
    assert!(msg.contains("no usable JDK was found"), "{msg}");
    assert!(msg.contains("jmods/java.base.jmod"), "{msg}");
    assert!(msg.contains("lib/modules"), "{msg}");
    // The message must offer the other mode explicitly rather than
    // silently taking it.
    assert!(msg.contains("--synthetic-jdk"), "{msg}");
}

// ── version banner ───────────────────────────────────────────────

/// gci i12/td: `-version` / `--version` wait for the VM (HotSpot creates
/// it first, so `-Xlog:gc+init` prints its block) only with an `-Xlog`
/// selection in the launcher section; the other queries never do. Since
/// the 2026-10-15 merge of dev the predicate is zr z9/hp's
/// [`version_query_boots_vm`] (switch on, class library present here).
#[test]
fn gci_i12td_version_waits_for_the_vm_only_with_an_xlog_selection() {
    let waits =
        |q: VersionQuery, parts: &[&str]| version_query_boots_vm(q, &tokens(parts), true, true);
    let g1_init = ["cratonvm", "-XX:+UseG1GC", "-Xlog:gc+init", "-version"];
    assert!(waits(VersionQuery::Version, &g1_init));
    assert!(waits(VersionQuery::Version, &["cratonvm", "-Xlog:gc", "--version"]));
    assert!(!waits(VersionQuery::Version, &["cratonvm", "-version"]));
    assert!(!waits(VersionQuery::Version, &["cratonvm", "-Xlog:disable", "-version"]));
    assert!(!waits(VersionQuery::Version, &["cratonvm", "-Xloggc:gc.log", "-version"]));
    // A program's own `-Xlog` argument is not the launcher's.
    assert!(!waits(
        VersionQuery::Version,
        &["cratonvm", "-version", "Main", "--", "-Xlog:gc"]
    ));
    for query in [VersionQuery::Full, VersionQuery::Internal, VersionQuery::Show] {
        assert!(!waits(query, &g1_init), "{query:?}");
    }
}

#[test]
fn version_query_is_recognised_in_the_launcher_section() {
    for (tok, expected) in [
        ("-version", VersionQuery::Version),
        ("--version", VersionQuery::Version),
        ("-fullversion", VersionQuery::Full),
        ("-Xinternalversion", VersionQuery::Internal),
        ("-showversion", VersionQuery::Show),
    ] {
        let got = scan_version_query(&tokens(&["cratonvm", tok]));
        assert_eq!(got.map(|(q, _)| q), Some(expected), "token {tok}");
    }
    // After the program selector these are program args.
    assert!(scan_version_query(&tokens(&["cratonvm", "Main", "--", "-version"])).is_none());
    assert!(scan_version_query(&tokens(&["cratonvm", "Main", "--"])).is_none());
}

/// zs s3/ob2: HotSpot 25.0.3's GC thread value syntax and its refusal
/// of a malformed value, in command-line order, on ZGC.
#[test]
fn zs3ob2_malformed_gc_thread_values_follow_hotspot() {
    // HotSpot 25.0.3's value syntax (observed locally).
    for (v, want) in [
        ("3", Some(3)),
        ("00", Some(0)),
        ("0x10", Some(16)),
        ("0X1F", Some(31)),
        ("3k", Some(3072)),
        ("1K", Some(1024)),
        ("1g", Some(1 << 30)),
        ("4294967295", Some(u32::MAX)),
        ("abc", None),
        ("-1", None),
        ("+3", None),
        ("1.5", None),
        ("", None),
        ("0x", None),
        ("4294967296", None),
        ("99999999999999999999", None),
        ("k", None),
    ] {
        assert_eq!(hotspot_parse_uint_flag(v), want, "{v:?}");
    }
    let argv = |a: &[&str]| -> Vec<String> {
        std::iter::once("cratonvm")
            .chain(a.iter().copied())
            .map(String::from)
            .collect()
    };
    // The first malformed one in command-line order, even if overridden.
    assert_eq!(
        scan_malformed_gc_thread_flag(&argv(&[
            "-XX:ConcGCThreads=y",
            "-XX:ParallelGCThreads=x",
            "-version"
        ])),
        Some("ConcGCThreads=y".to_string())
    );
    assert_eq!(
        scan_malformed_gc_thread_flag(&argv(&["-XX:ConcGCThreads=abc", "-XX:ConcGCThreads=2"])),
        Some("ConcGCThreads=abc".to_string())
    );
    assert_eq!(
        scan_malformed_gc_thread_flag(&argv(&[
            "-XX:ParallelGCThreads=0x10",
            "-XX:ConcGCThreads=0"
        ])),
        None
    );
    // Program arguments (after `--`) are not launcher options.
    assert_eq!(
        scan_malformed_gc_thread_flag(&argv(&["Main", "--", "-XX:ConcGCThreads=abc"])),
        None
    );
    // The collector: explicit, default, deselected, unsupported.
    assert!(scan_effective_zgc(&argv(&["-XX:+UseZGC"])));
    assert_eq!(
        scan_effective_zgc(&argv(&["-version"])),
        cfg!(feature = "zgc")
    );
    assert!(!scan_effective_zgc(&argv(&["-XX:+UseG1GC"])));
    assert!(!scan_effective_zgc(&argv(&["-XX:+UseZGC", "-XX:-UseZGC"])));
    assert!(!scan_effective_zgc(&argv(&["-XX:+UseSerialGC"])));
}

/// zs s2/ob: HotSpot 25.0.3's refusal of `-XX:+UseZGC` with a zero GC
/// thread count (ParallelGCThreads checked first, `00` is zero, an
/// implicit ZGC or a later `-XX:-UseZGC` is not refused, `=0` kill).
/// zs s9/ob6: an implicit ZGC refuses a zero `ParallelGCThreads` with
/// HotSpot's default-G1 text (local 25.0.3: `java -XX:ParallelGCThreads=0
/// -version` -> stdout `Error occurred during initialization of VM` /
/// `The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0`,
/// exit 1); not a zero `ConcGCThreads`, not with any collector flag, not
/// with the kill.
#[test]
fn zs9ob6_implicit_zgc_refuses_zero_parallel_gc_threads_as_hotspot_g1() {
    let msg = Some(
        "The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0".to_string(),
    );
    let refuse = zgc_implicit_zero_parallel_gc_threads_refusal;
    assert_eq!(refuse(true, Some("0"), true, true), msg);
    assert_eq!(refuse(true, Some("0x0"), true, true), msg);
    assert_eq!(refuse(true, Some("0x0"), true, false), None, "decimal only");
    assert_eq!(refuse(true, Some("2"), true, true), None);
    assert_eq!(refuse(true, None, true, true), None);
    assert_eq!(refuse(false, Some("0"), true, true), None, "explicit");
    assert_eq!(refuse(true, Some("0"), false, true), None, "=0 kill");
    let scan = |parts: &[&str]| {
        let mut argv = vec!["cratonvm"];
        argv.extend_from_slice(parts);
        scan_zgc_implicit_zero_parallel_gc_threads_refusal(&tokens(&argv))
    };
    let p0 = "-XX:ParallelGCThreads=0";
    if cfg!(feature = "zgc") {
        assert_eq!(scan(&[p0, "-version"]), msg);
    }
    assert_eq!(scan(&["-XX:ConcGCThreads=0", "-version"]), None);
    assert_eq!(scan(&["-XX:+UseZGC", p0, "-version"]), None);
    assert_eq!(scan(&["-XX:+UseG1GC", p0, "-version"]), None);
    assert_eq!(scan(&["-XX:-UseZGC", p0, "-version"]), None);
}

#[test]
fn zs2ob_explicit_zgc_refuses_zero_gc_thread_counts() {
    let msg = |n: &str| {
        Some(format!(
            "The flag -XX:+UseZGC can not be combined with -XX:{n}=0"
        ))
    };
    let refuse =
        |zgc, parallel, conc, on| zgc_zero_gc_thread_refusal(zgc, parallel, conc, on, true);
    assert_eq!(
        refuse(true, Some("0"), None, true),
        msg("ParallelGCThreads")
    );
    assert_eq!(refuse(true, None, Some("00"), true), msg("ConcGCThreads"));
    assert_eq!(
        refuse(true, Some("0"), Some("0"), true),
        msg("ParallelGCThreads")
    );
    assert_eq!(refuse(true, Some("2"), Some("1"), true), None);
    assert_eq!(refuse(true, Some("x"), None, true), None);
    assert_eq!(refuse(false, Some("0"), Some("0"), true), None);
    assert_eq!(refuse(true, Some("0"), None, false), None);
    let scan = |parts: &[&str]| {
        let mut argv = vec!["cratonvm"];
        argv.extend_from_slice(parts);
        scan_zgc_zero_gc_thread_refusal(&tokens(&argv))
    };
    let (zgc, conc0) = ("-XX:+UseZGC", "-XX:ConcGCThreads=0");
    assert_eq!(scan(&[zgc, conc0, "-version"]), msg("ConcGCThreads"));
    assert_eq!(scan(&[conc0, "-version"]), None, "implicit ZGC");
    assert_eq!(scan(&[zgc, "-XX:-UseZGC", conc0, "-version"]), None);
    assert_eq!(
        scan(&["-XX:+UseG1GC", "-XX:ParallelGCThreads=0", "-version"]),
        None
    );
    assert_eq!(
        scan(&[zgc, "-version", "--", conc0]),
        None,
        "a program argument"
    );
}

/// zs s4/rv3: a zero in HotSpot's value syntax (`0x0`, `0X0`, `0k`,
/// `0x0k`) is the zero an explicit `-XX:+UseZGC` refuses, on the parsed
/// path and on the version scan; with the syntax switch off only a
/// decimal zero is. Before the fix each of them passed the decimal-only
/// zero test and was then accepted (`ParallelGCThreads`) or dropped in
/// silence (`ConcGCThreads`).
#[test]
fn zs4rv3_a_hotspot_spelled_zero_is_refused_on_explicit_zgc() {
    let msg = |n: &str| {
        Some(format!(
            "The flag -XX:+UseZGC can not be combined with -XX:{n}=0"
        ))
    };
    for z in ["0x0", "0X0", "0k", "0x0k", "00"] {
        assert_eq!(
            zgc_zero_gc_thread_refusal(true, Some(z), None, true, true),
            msg("ParallelGCThreads"),
            "{z}"
        );
        assert_eq!(
            zgc_zero_gc_thread_refusal(true, None, Some(z), true, true),
            msg("ConcGCThreads"),
            "{z}"
        );
        assert_eq!(
            zgc_zero_gc_thread_refusal(false, Some(z), Some(z), true, true),
            None,
            "implicit ZGC: {z}"
        );
    }
    assert_eq!(
        zgc_zero_gc_thread_refusal(true, Some("0x0"), None, true, false),
        None,
        "decimal-only with the syntax switch off"
    );
    assert_eq!(
        zgc_zero_gc_thread_refusal(true, Some("0x10"), Some("1k"), true, true),
        None
    );
    {
        let mut argv = vec![
            "cratonvm",
            "-XX:+UseZGC",
            "-XX:ConcGCThreads=0x0",
            "-version",
        ];
        assert_eq!(
            scan_zgc_zero_gc_thread_refusal(&tokens(&argv)),
            msg("ConcGCThreads")
        );
        argv[2] = "-XX:ParallelGCThreads=0k";
        assert_eq!(
            scan_zgc_zero_gc_thread_refusal(&tokens(&argv)),
            msg("ParallelGCThreads")
        );
    }
}

/// zr z9/hp: `-version` boots the VM (so `-Xlog` startup lines print, as
/// on HotSpot) only with an `-Xlog` option in the launcher section, the
/// class library present and the switch on; every other banner keeps the
/// no-VM fast path.
#[test]
fn zr9hp_version_boots_the_vm_only_when_xlog_selects_output() {
    let boots = |q: VersionQuery, parts: &[&str], switch_on: bool, library: bool| {
        version_query_boots_vm(q, &tokens(parts), switch_on, library)
    };
    let v = VersionQuery::Version;
    let init = ["cratonvm", "-Xlog:gc+init", "-version"];
    let bare = ["cratonvm", "-version", "-Xlog"];
    let file = ["cratonvm", "-Xlog:gc*:file=g.log", "--version"];
    let gc = ["cratonvm", "-Xlog:gc", "-version"];
    assert!(boots(v, &init, true, true));
    assert!(boots(v, &bare, true, true));
    assert!(boots(v, &file, true, true));
    assert!(boots(v, &gc, true, true));
    // No -Xlog: nothing to log, fast path.
    assert!(!boots(v, &["cratonvm", "-version"], true, true));
    // An -Xlog that is a program argument does not count.
    let program_arg = ["cratonvm", "-version", "Main", "--", "-Xlog:gc"];
    assert!(!boots(v, &program_arg, true, true));
    // A look-alike option is not -Xlog.
    assert!(!boots(v, &["cratonvm", "-Xlogfoo", "-version"], true, true));
    // Kill switch and a missing class library.
    assert!(!boots(v, &gc, false, true));
    assert!(!boots(v, &gc, true, false));
    // The other banners never boot here.
    let others = [
        VersionQuery::Full,
        VersionQuery::Internal,
        VersionQuery::Show,
    ];
    for q in others {
        assert!(!boots(q, &gc, true, true));
    }
}

#[test]
fn early_nojit_scan_ignores_java_program_arguments() {
    assert!(launcher_nojit_requested(&tokens(&[
        "cratonvm", "--nojit", "Main", "--"
    ])));
    assert!(!launcher_nojit_requested(&tokens(&[
        "cratonvm", "Main", "--", "--nojit"
    ])));
}

/// Wave 138, lane L1b: `-Xint` (last of `-Xint` / `-Xmixed` / `-Xcomp`)
/// asks for the interpreter; a program argument does not. (The
/// `CRATONVM_LAUNCHER_XINT=0` kill is read from the environment by
/// `launcher_xint_enabled`, not tested here.)
#[test]
fn i138_l1b_early_scan_maps_xint_to_the_interpreter() {
    assert!(launcher_xint_requested(&tokens(&["cratonvm", "-Xint", "Main", "--"])));
    assert!(!launcher_xint_requested(&tokens(&["cratonvm", "-Xint", "-Xmixed", "Main", "--"])));
    assert!(launcher_xint_requested(&tokens(&["cratonvm", "-Xcomp", "-Xint", "Main", "--"])));
    assert!(!launcher_xint_requested(&tokens(&["cratonvm", "-Xcomp", "Main", "--"])));
    assert!(!launcher_xint_requested(&tokens(&["cratonvm", "Main", "--", "-Xint"])));
    let banner = version_banner_with(
        VersionQuery::Version,
        JdkMode::Real,
        CompatibilityMode::Compatible,
        None,
        true,
    );
    assert!(banner.contains(", interpreted mode, compatibility=compatible)"), "{banner}");
}

/// The launcher-side scan for `--dump-phase-report`. It has to run before
/// the flag snapshot is latched, so it cannot go through clap; this pins
/// both accepted spellings and the `--` boundary that keeps a Java program
/// argument of the same name from being consumed.
#[test]
fn phase_report_scan_accepts_both_spellings_and_stops_at_the_separator() {
    assert_eq!(
        launcher_phase_report_path(&tokens(&[
            "cratonvm",
            "--dump-phase-report",
            "/tmp/p.json",
            "Main",
            "--"
        ])),
        Some("/tmp/p.json".to_string())
    );
    assert_eq!(
        launcher_phase_report_path(&tokens(&[
            "cratonvm",
            "--dump-phase-report=/tmp/p.json",
            "Main",
            "--"
        ])),
        Some("/tmp/p.json".to_string())
    );
    // Past the separator it is the Java program's argument, not ours.
    assert_eq!(
        launcher_phase_report_path(&tokens(&[
            "cratonvm",
            "Main",
            "--",
            "--dump-phase-report",
            "/tmp/p.json"
        ])),
        None
    );
    // A trailing option with no value is not a panic and not a guess.
    assert_eq!(
        launcher_phase_report_path(&tokens(&["cratonvm", "--dump-phase-report"])),
        None
    );
    assert_eq!(
        launcher_phase_report_path(&tokens(&["cratonvm", "Main"])),
        None
    );
}

/// The launcher consumes the option, but clap must still accept it or the
/// parse in `run()` would fail with "unexpected argument".
#[test]
fn clap_accepts_the_phase_report_option() {
    let parsed = Args::try_parse_from(tokens(&[
        "cratonvm",
        "--dump-phase-report",
        "/tmp/p.json",
        "Main",
    ]))
    .expect("clap must accept --dump-phase-report");
    assert_eq!(parsed.dump_phase_report.as_deref(), Some("/tmp/p.json"));
}

/// The whole point of routing version output through our own banner:
/// it must name the active class library.
#[test]
fn version_banner_names_the_jdk_mode() {
    let banner = version_banner(
        VersionQuery::Version,
        JdkMode::Synthetic,
        CompatibilityMode::Compatible,
        None,
    );
    assert!(banner.contains("cratonvm version"), "{banner}");
    assert!(banner.contains("synthetic-jdk"), "{banner}");
    let banner = version_banner(
        VersionQuery::Version,
        JdkMode::Real,
        CompatibilityMode::Compatible,
        None,
    );
    assert!(banner.contains("real-jdk"), "{banner}");
}

#[test]
fn internal_version_reports_both_defaults_and_the_search_path() {
    let banner = version_banner(
        VersionQuery::Internal,
        JdkMode::Real,
        CompatibilityMode::Compatible,
        None,
    );
    for needle in [
        "jdk.mode.active",
        "jdk.compatibility.mode",
        "jdk.mode.default.launcher",
        "jdk.mode.default.embedded",
        "jdk.mode.synthetic_compiled_in",
        "never host-detected",
        "JAVA_HOME",
        "java on PATH",
    ] {
        assert!(banner.contains(needle), "missing {needle}:\n{banner}");
    }
}

#[test]
fn version_stream_matches_hotspot_conventions() {
    // `java -version` → stderr (build tools scrape it there);
    // `java --version` → stdout.
    assert!(!VersionQuery::Version.to_stdout("-version"));
    assert!(VersionQuery::Version.to_stdout("--version"));
    assert!(VersionQuery::Version.exits());
    assert!(!VersionQuery::Show.exits());
}

#[test]
fn showversion_token_is_stripped_before_clap() {
    let mut argv = tokens(&["cratonvm", "-showversion", "Main", "--", "-showversion"]);
    remove_first_launcher_token(&mut argv, "-showversion");
    // Only the launcher-section occurrence is removed; the program arg
    // after `--` survives.
    assert_eq!(argv, tokens(&["cratonvm", "Main", "--", "-showversion"]));
}

#[test]
fn explicit_java_home_is_visible_to_the_banner() {
    assert_eq!(
        scan_explicit_java_home(&tokens(&[
            "cratonvm",
            "--java-home",
            "/opt/jdk",
            "Main",
            "--"
        ])),
        Some("/opt/jdk".to_string())
    );
    assert_eq!(
        scan_explicit_java_home(&tokens(&["cratonvm", "--java-home=/opt/jdk", "Main", "--"])),
        Some("/opt/jdk".to_string())
    );
    assert_eq!(
        scan_explicit_java_home(&tokens(&["cratonvm", "Main", "--", "--java-home", "/x"])),
        None
    );
}

/// Both spellings of every JPMS `--add-*` flag must parse, and must not
/// swallow the following argument.
///
/// HotSpot accepts `--add-opens M/P=T` and `--add-opens=M/P=T` alike. A
/// launcher that consumed the NEXT token as part of a space-separated
/// value would eat `-cp` and its directory, and the symptom would be
/// `Could not find or load main class` — which reads as a broken program
/// or a bad classpath, not as a mis-parsed flag. That misreading is not
/// hypothetical: W7-56-infercaller-strict-RETIRED-20260917.md reported exactly this defect
/// and told readers to prefer the `=` spelling on CratonVM. It was wrong;
/// re-measured 2026-08-12, all eight combinations below already worked, on
/// the then-current binary AND on the pre-merge control. These assertions
/// exist so the claim can be settled by running the tests rather than by
/// re-deriving it, and so a future arg-parsing change cannot make it true.
///
/// The `-cp` and main-class assertions are the load-bearing half: a test
/// that only checked the flag's own value would pass against a launcher
/// that swallowed the classpath.
///
/// Parsed through `normalize_java_launcher_argv`, which is what the real
/// launcher does before clap sees anything. Calling `Args::try_parse_from`
/// directly is NOT equivalent and would test the wrong thing: clap reads a
/// bare `-cp /tmp/cp` as short `-c` with the attached value `p`, so the
/// assertion below fails for a reason that has nothing to do with the flag
/// under test. Measured while writing this test.
#[test]
fn add_star_flags_accept_both_spellings_without_eating_the_next_arg() {
    // (flag, value, accessor label) — one row per JPMS add-* flag.
    let cases: &[(&str, &str)] = &[
        ("--add-opens", "java.logging/java.util.logging=ALL-UNNAMED"),
        ("--add-exports", "java.base/java.lang=ALL-UNNAMED"),
        ("--add-reads", "java.logging=ALL-UNNAMED"),
        ("--add-modules", "java.logging"),
    ];
    for (flag, value) in cases {
        // Space-separated: `--flag VALUE -cp DIR Main`
        let spaced = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
            "cratonvm", flag, value, "-cp", "/tmp/cp", "Main",
        ])))
        .unwrap_or_else(|e| panic!("clap must accept `{flag} {value}`: {e}"));

        // Joined: `--flag=VALUE -cp DIR Main`
        let joined_flag = format!("{flag}={value}");
        let joined = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
            "cratonvm",
            &joined_flag,
            "-cp",
            "/tmp/cp",
            "Main",
        ])))
        .unwrap_or_else(|e| panic!("clap must accept `{joined_flag}`: {e}"));

        for (spelling, parsed) in [("space", &spaced), ("equals", &joined)] {
            // The flag's own value survived.
            let got: &[String] = match *flag {
                "--add-opens" => &parsed.add_opens,
                "--add-exports" => &parsed.add_exports,
                "--add-reads" => &parsed.add_reads,
                "--add-modules" => &parsed.add_modules,
                other => unreachable!("unlisted flag {other}"),
            };
            assert_eq!(
                got,
                &[value.to_string()],
                "{flag} ({spelling}) lost or mangled its own value"
            );

            // ...and, the half that actually catches the reported defect,
            // the FOLLOWING option was not consumed as part of it.
            assert_eq!(
                parsed.classpath.as_deref(),
                Some("/tmp/cp"),
                "{flag} ({spelling}) swallowed -cp; this is the defect that surfaces as `Could not find or load main class`"
            );
            assert_eq!(
                parsed.class_name.as_deref(),
                Some("Main"),
                "{flag} ({spelling}) swallowed the main class"
            );
        }
    }
}

/// `-XX:MaxJavaStackTraceDepth=N` reaches clap instead of being dropped as
/// an unimplemented `-XX:` knob, and does not eat the following option.
#[test]
fn max_java_stack_trace_depth_is_parsed() {
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
        "cratonvm",
        "-XX:MaxJavaStackTraceDepth=0",
        "-cp",
        "/tmp/cp",
        "Main",
    ])))
    .expect("clap must accept -XX:MaxJavaStackTraceDepth=0");
    assert_eq!(parsed.max_java_stack_trace_depth.as_deref(), Some("0"));
    assert_eq!(parsed.classpath.as_deref(), Some("/tmp/cp"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

/// gen r5w2/conc6: `-XX:SoftRefLRUPolicyMSPerMB=N` reaches clap instead of
/// being dropped as an unimplemented `-XX:` knob, does not eat the
/// following option, and its value parses as a non-negative integer.
#[test]
fn soft_ref_lru_policy_ms_per_mb_is_parsed() {
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
        "cratonvm",
        "-XX:SoftRefLRUPolicyMSPerMB=0",
        "-cp",
        "/tmp/cp",
        "Main",
    ])))
    .expect("clap must accept -XX:SoftRefLRUPolicyMSPerMB=0");
    assert_eq!(parsed.soft_ref_lru_policy_ms_per_mb.as_deref(), Some("0"));
    assert_eq!(parsed.classpath.as_deref(), Some("/tmp/cp"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parse_soft_ref_lru_policy("0"), Some(0));
    assert_eq!(parse_soft_ref_lru_policy(" 250 "), Some(250));
    assert_eq!(parse_soft_ref_lru_policy("-1"), None);
    assert_eq!(parse_soft_ref_lru_policy("1k"), None);
}

/// gcg g3/x: HotSpot's heap-size and free-ratio `-XX:` flags reach clap
/// (they were dropped by the silent `-XX:` catch-all): `MaxHeapSize` is
/// `-Xmx` (last one wins, in argv order), the other four have their own
/// options, and none eats the following option.
#[test]
fn gcg_g3x_heap_size_and_free_ratio_flags_are_parsed() {
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
        "cratonvm",
        "-Xmx1g",
        "-XX:MaxHeapSize=64m",
        "-XX:MinHeapSize=12m",
        "-XX:InitialHeapSize=32m",
        "-XX:MinHeapFreeRatio=5",
        "-XX:MaxHeapFreeRatio=10",
        "-cp",
        "/tmp/cp",
        "Main",
    ])))
    .expect("clap must accept the heap-size and free-ratio flags");
    assert_eq!(parsed.max_heap.as_deref(), Some("64m"));
    assert_eq!(parsed.min_heap_size.as_deref(), Some("12m"));
    assert_eq!(parsed.initial_heap_size_xx.as_deref(), Some("32m"));
    assert_eq!(parsed.min_heap_free_ratio.as_deref(), Some("5"));
    assert_eq!(parsed.max_heap_free_ratio.as_deref(), Some("10"));
    assert_eq!(parsed.classpath.as_deref(), Some("/tmp/cp"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
        "cratonvm",
        "-XX:MaxHeapSize=64m",
        "-Xmx128m",
        "Main",
    ])))
    .expect("clap must accept -XX:MaxHeapSize then -Xmx");
    assert_eq!(
        parsed.max_heap.as_deref(),
        Some("128m"),
        "the later one wins"
    );
}

/// gcg g4/yg (review item 10): Serial's 2 MiB heap alignment, the
/// measured HotSpot 25.0.3 cases, and the clamps.
#[test]
fn gcg_g4yg_heap_sizes_align_up_to_two_mib_as_serial_does() {
    const M: usize = 1024 * 1024;
    assert_eq!(hotspot_heap_align_up(9 * M), 10 * M);
    assert_eq!(hotspot_heap_align_up(100_000 * 1024), 98 * M);
    assert_eq!(hotspot_heap_align_up(5_000 * 1024), 6 * M);
    assert_eq!(
        hotspot_heap_align_up(64 * M),
        64 * M,
        "an aligned size is unchanged"
    );
    assert_eq!(hotspot_heap_align_up(0), 0);
    assert_eq!(hotspot_heap_align_up(usize::MAX), usize::MAX & !(2 * M - 1));
    assert_eq!(
        hotspot_aligned_heap_sizes(9 * M, 16 * M, Some(5_000 * 1024), 2 * M),
        (10 * M, 10 * M, Some(6 * M)),
        "initial and min never exceed the aligned max"
    );
    assert_eq!(
        hotspot_aligned_heap_sizes(64 * M, 16 * M, None, 2 * M),
        (64 * M, 16 * M, None)
    );
}

/// gci zl1 (zx48 A4): `CRATONVM_GC_HEAP_ALIGN_HOTSPOT` on every collector
/// -- HotSpot 25's `HeapAlignment`: Serial and ZGC 2 MiB, G1
/// `max(region, 2 MiB)` with the region from the unaligned `-Xmx` or
/// `-XX:G1HeapRegionSize` (rounded up to a power of two).
#[test]
fn gci_zl1_heap_alignment_covers_every_collector() {
    use cratonvm_vm::config::GcAlgorithm;
    const M: usize = 1024 * 1024;
    let align = |gc, max, region| hotspot_heap_alignment(gc, max, region);
    assert_eq!(align(GcAlgorithm::Generational, 9 * M, None), Some(2 * M));
    assert_eq!(align(GcAlgorithm::Zgc, 9 * M, None), Some(2 * M));
    // G1 -Xmx9m: 1M regions, so the card table's 2 MiB wins.
    assert_eq!(align(GcAlgorithm::G1, 9 * M, None), Some(2 * M));
    // G1 -Xmx16g: 8M regions.
    assert_eq!(align(GcAlgorithm::G1, 16384 * M, None), Some(8 * M));
    // An explicit region size wins, rounded up to a power of two.
    assert_eq!(align(GcAlgorithm::G1, 9 * M, Some(3 * M)), Some(4 * M));
    assert_eq!(hotspot_heap_align_up_to(9 * M, 4 * M), 12 * M);
    assert_eq!(hotspot_heap_align_up_to(usize::MAX, 8 * M), usize::MAX & !(8 * M - 1));
    assert_eq!(hotspot_heap_align_up_to(5, 0), 5, "0 is taken as 1");
    // The -Xmx9m rows: a 10M heap on every collector at 2 MiB.
    for gc in [GcAlgorithm::Generational, GcAlgorithm::G1, GcAlgorithm::Zgc] {
        let a = align(gc, 9 * M, None).expect("every collector is modelled");
        assert_eq!(
            hotspot_aligned_heap_sizes(9 * M, 16 * M, None, a),
            (10 * M, 10 * M, None),
            "{gc:?}"
        );
    }
    assert_eq!(
        hotspot_aligned_heap_sizes(9 * M, 5 * M, Some(M), 4 * M),
        (12 * M, 8 * M, Some(4 * M))
    );
}

/// gcg g4/yg (`gcg-g3x-proposal-hotspot-ergonomic-initial-heap`): HotSpot's
/// `physical / 64`, clamped and aligned, on the measured 16 GiB box.
#[test]
fn gcg_g4yg_ergonomic_initial_heap_is_hotspots() {
    const M: usize = 1024 * 1024;
    let box_16g: u64 = 16_233 * 1024 * 1024;
    assert_eq!(
        hotspot_ergonomic_initial_heap(box_16g, None, 64 * M),
        64 * M
    );
    assert_eq!(
        hotspot_ergonomic_initial_heap(box_16g, None, 512 * M),
        254 * M
    );
    assert_eq!(
        hotspot_ergonomic_initial_heap(box_16g, None, 10 * M),
        10 * M
    );
    // `MinHeapSize` is the floor on a small box.
    assert_eq!(
        hotspot_ergonomic_initial_heap(64 * M as u64, Some(12 * M), 512 * M),
        12 * M
    );
    assert_eq!(
        hotspot_ergonomic_initial_heap(64 * M as u64, None, 512 * M),
        2 * M
    );
}

/// gcg g3/x (`gcg-g2e-heap-free-ratio-flags-are-not-parsed`): the free
/// ratios are validated with HotSpot 25.0.3's exact refusal lines.
#[test]
fn gcg_g3x_heap_free_ratios_are_validated_as_hotspot_does() {
    assert_eq!(heap_free_ratios_from_flags(None, None), Ok((None, None)));
    assert_eq!(
        heap_free_ratios_from_flags(Some("5"), Some("10")),
        Ok((Some(5), Some(10)))
    );
    assert_eq!(
        heap_free_ratios_from_flags(Some("0x10"), None),
        Ok((Some(16), None))
    );
    assert_eq!(
        heap_free_ratios_from_flags(Some("100"), Some("100")),
        Ok((Some(100), Some(100)))
    );
    assert_eq!(
        heap_free_ratios_from_flags(Some("50"), Some("20")),
        Err(vec![
            "MinHeapFreeRatio (50) must be less than or equal to MaxHeapFreeRatio (20)"
                .to_string(),
            "MaxHeapFreeRatio (20) must be greater than or equal to MinHeapFreeRatio (50)"
                .to_string(),
        ])
    );
    // A lone `MaxHeapFreeRatio=10` is below the default minimum, 40.
    assert_eq!(
        heap_free_ratios_from_flags(None, Some("10")),
        Err(vec![
            "MinHeapFreeRatio (40) must be less than or equal to MaxHeapFreeRatio (10)"
                .to_string(),
            "MaxHeapFreeRatio (10) must be greater than or equal to MinHeapFreeRatio (40)"
                .to_string(),
        ])
    );
    assert_eq!(
        heap_free_ratios_from_flags(None, Some("101")),
        Err(vec![
            "uintx MaxHeapFreeRatio=101 is outside the allowed range [ 0 ... 100 ]".to_string(),
            "Improperly specified VM option 'MaxHeapFreeRatio=101'".to_string(),
        ])
    );
    assert_eq!(
        heap_free_ratios_from_flags(Some("-1"), None),
        Err(vec![
            "Improperly specified VM option 'MinHeapFreeRatio=-1'".to_string()
        ])
    );
    assert_eq!(
        heap_free_ratios_from_flags(Some("x"), None),
        Err(vec![
            "Improperly specified VM option 'MinHeapFreeRatio=x'".to_string()
        ])
    );
}

/// gci i11/gm finding 13: `-XX:G1HeapRegionSize` above HotSpot 25's 512 MiB
/// range refuses to start with HotSpot's range line; 512 MiB itself, sub-MiB
/// values (the collector rounds them up to 1 MiB) and 0 (ergonomic) pass.
#[test]
fn gci_i11gm_g1_heap_region_size_range_matches_hotspot() {
    assert_eq!(
        g1_heap_region_size_from_flag("1g"),
        Err(vec![
            "size_t G1HeapRegionSize=1073741824 is outside the allowed range [ 0 ... 536870912 ]"
                .to_string(),
            "Improperly specified VM option 'G1HeapRegionSize=1g'".to_string(),
        ])
    );
    assert_eq!(g1_heap_region_size_from_flag("512m"), Ok(Some(512 * 1024 * 1024)));
    assert_eq!(g1_heap_region_size_from_flag("512k"), Ok(Some(512 * 1024)));
    assert_eq!(g1_heap_region_size_from_flag("64m"), Ok(Some(64 * 1024 * 1024)));
    assert_eq!(g1_heap_region_size_from_flag("0"), Ok(None));
    assert_eq!(g1_heap_region_size_from_flag("x"), Ok(None));
}

/// clap must reject the two mode flags together rather than letting one
/// silently win.
#[test]
fn clap_rejects_both_mode_flags() {
    assert!(
        Args::try_parse_from(tokens(&[
            "cratonvm",
            "--real-jdk",
            "--synthetic-jdk",
            "Main"
        ]))
        .is_err(),
        "--real-jdk and --synthetic-jdk must conflict"
    );
    let parsed = Args::try_parse_from(tokens(&["cratonvm", "--real-jdk", "Main"]))
        .expect("clap must accept --real-jdk");
    assert!(parsed.real_jdk);
    assert!(!parsed.synthetic_jdk);
}

// -----------------------------------------------------------------------
// Ergonomic default-heap clamp — pure math, exercised directly so the
// container-vs-host basis logic is covered without a real cgroupfs.
// -----------------------------------------------------------------------

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[test]
fn ergo_clamp_quarter_of_large_host() {
    // 16 GiB basis → 1/4 = 4 GiB, under the default cap.
    assert_eq!(
        clamp_ergonomic_heap(16 * GIB, MAX_ERGONOMIC_HEAP),
        4 * GIB as usize
    );
}

#[test]
fn ergo_clamp_capped_for_huge_host() {
    // 64 GiB basis → 1/4 = 16 GiB, as HotSpot answers (wave 48: the
    // cap is HotSpot's 32 GiB, no longer 4 GiB).
    assert_eq!(
        clamp_ergonomic_heap(64 * GIB, MAX_ERGONOMIC_HEAP),
        16 * GIB as usize
    );
    // 256 GiB basis → 1/4 = 64 GiB, capped to the 32 GiB default.
    assert_eq!(
        clamp_ergonomic_heap(256 * GIB, MAX_ERGONOMIC_HEAP),
        32 * GIB as usize
    );
}

#[test]
fn ergo_clamp_floored_small_basis() {
    // 4 GiB basis → 1/4 = 1 GiB (above the 256 MiB floor).
    assert_eq!(
        clamp_ergonomic_heap(4 * GIB, MAX_ERGONOMIC_HEAP),
        GIB as usize
    );
    // 512 MiB basis → 1/4 = 128 MiB, raised to the 256 MiB floor.
    assert_eq!(
        clamp_ergonomic_heap(512 * MIB, MAX_ERGONOMIC_HEAP),
        (256 * MIB) as usize
    );
}

#[test]
fn ergo_clamp_never_exceeds_basis() {
    // A 256 MiB container: 1/4 = 64 MiB, the floor would push it to
    // 256 MiB — but it must never exceed the basis itself, so it stays
    // at exactly 256 MiB (not above), and a 200 MiB basis stays at 200.
    assert_eq!(
        clamp_ergonomic_heap(256 * MIB, MAX_ERGONOMIC_HEAP),
        (256 * MIB) as usize
    );
    assert_eq!(
        clamp_ergonomic_heap(200 * MIB, MAX_ERGONOMIC_HEAP),
        (200 * MIB) as usize
    );
}

#[test]
fn ergo_clamp_custom_cap_floored() {
    // A tiny cap override can't drop the result below the 256 MiB floor.
    assert_eq!(
        clamp_ergonomic_heap(16 * GIB, 64 * MIB),
        (256 * MIB) as usize
    );
    // A 2 GiB cap bites on a big host (8 GiB → 1/4 = 2 GiB).
    assert_eq!(clamp_ergonomic_heap(8 * GIB, 2 * GIB), (2 * GIB) as usize);
}

#[test]
fn watchdog_is_not_armed_by_default() {
    assert_eq!(resolve_watchdog_timeout(None, None), None);
}

#[test]
fn watchdog_env_default_is_opt_in() {
    assert_eq!(resolve_watchdog_timeout(None, Some("300")), Some(300));
    assert_eq!(resolve_watchdog_timeout(None, Some("0")), None);
    assert_eq!(resolve_watchdog_timeout(None, Some("not-a-number")), None);
}

#[test]
fn watchdog_explicit_timeout_wins_and_zero_disables() {
    assert_eq!(resolve_watchdog_timeout(Some(7), Some("300")), Some(7));
    assert_eq!(resolve_watchdog_timeout(Some(0), Some("300")), None);
}

// -----------------------------------------------------------------------
// insert_program_args_separator tests — `java`-launcher positional
// semantics: tokens after the program selector are program args.
// -----------------------------------------------------------------------

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[test]
fn sep_jar_then_program_help_is_program_arg() {
    // `-jar foo.jar --help`: `--help` must become a program arg.
    let out = insert_program_args_separator(argv(&["java", "-jar", "foo.jar", "--help"]));
    assert_eq!(out, argv(&["java", "-jar", "foo.jar", "--", "--help"]));
}

#[test]
fn sep_jar_with_leading_opts() {
    // Launcher options before `-jar` stay in the leading section.
    let out = insert_program_args_separator(argv(&[
        "java",
        "--java-home",
        "C:/jdk",
        "-jar",
        "app.jar",
        "--list-modules",
        "--version",
    ]));
    assert_eq!(
        out,
        argv(&[
            "java",
            "--java-home",
            "C:/jdk",
            "-jar",
            "app.jar",
            "--",
            "--list-modules",
            "--version",
        ])
    );
}

#[test]
fn sep_bare_main_class_then_program_args() {
    // `-cp bench Main --help 0`: `Main` selects the program; everything
    // after it (including `--help`) is a program arg.
    let out =
        insert_program_args_separator(argv(&["java", "-cp", "bench", "Main", "--help", "0"]));
    assert_eq!(
        out,
        argv(&["java", "-cp", "bench", "Main", "--", "--help", "0"])
    );
}

// -----------------------------------------------------------------------
// misplaced_launcher_flags — the silent-ignore family (G33-1 defect 2).
// The detector is pure and reads the argv `insert_program_args_separator`
// produced, so these compose the two functions exactly as `main()` does.
// -----------------------------------------------------------------------

#[test]
fn misplaced_dump_flag_after_main_class_is_detected() {
    // The exact shape that cost this campaign time twice: the census flag
    // parked behind the main class. The launcher discards it, the VM exits
    // 0 and writes nothing, and before this warning nothing said so.
    let out = insert_program_args_separator(argv(&[
        "cratonvm",
        "-cp",
        "build",
        "RJdkHello",
        "--dump-native-registry",
        "reg.json",
    ]));
    assert_eq!(
        misplaced_launcher_flags(&out),
        vec!["--dump-native-registry"]
    );
}

#[test]
fn correctly_placed_flags_are_not_reported() {
    // The whole family, all ahead of the selector. A detector that fires
    // here would train every reader to ignore it, which is worse than not
    // having one.
    let out = insert_program_args_separator(argv(&[
        "cratonvm",
        "--real-jdk",
        "--nojit",
        "--dump-native-registry",
        "reg.json",
        "--real-jdk-report",
        "r.json",
        "-cp",
        "build",
        "Main",
        "5",
    ]));
    assert!(misplaced_launcher_flags(&out).is_empty());
}

#[test]
fn misplaced_flags_are_reported_once_each_in_argv_order() {
    let out = insert_program_args_separator(argv(&[
        "cratonvm",
        "-cp",
        "build",
        "Main",
        "--nojit",
        "--real-jdk",
        "--nojit",
        "--dump-native-registry=reg.json",
    ]));
    assert_eq!(
        misplaced_launcher_flags(&out),
        // Order is argv order, not list order, so the message reads in the
        // order the user typed. Deduplicated, because a repeated flag is
        // one mistake.
        vec!["--nojit", "--real-jdk", "--dump-native-registry"],
        "the inline `--flag=value` form must report as the bare flag"
    );
}

#[test]
fn misplaced_detection_covers_the_jar_form_too() {
    // `-jar app.jar` is the other program selector, and it is the form a
    // build tool is most likely to append flags to.
    let out = insert_program_args_separator(argv(&[
        "cratonvm",
        "-jar",
        "app.jar",
        "--real-jdk-report",
        "r.json",
    ]));
    assert_eq!(misplaced_launcher_flags(&out), vec!["--real-jdk-report"]);
}

#[test]
fn a_program_argument_that_merely_resembles_a_flag_is_not_reported() {
    // Only exact spellings from the list. A program arg that shares a
    // prefix, or a value that happens to look like one, must not fire —
    // the warning has to survive contact with real command lines.
    let out = insert_program_args_separator(argv(&[
        "cratonvm",
        "-cp",
        "build",
        "Main",
        "--dump-native-registry-v2",
        "--real-jdk-reporter",
        "--dump",
        "-nojit",
    ]));
    assert!(misplaced_launcher_flags(&out).is_empty());
}

#[test]
fn no_program_selector_means_nothing_is_misplaced() {
    // `cratonvm --real-jdk --version`: no separator is inserted at all, so
    // every token is still a launcher option and none is discarded.
    let out = insert_program_args_separator(argv(&["cratonvm", "--real-jdk", "--version"]));
    assert!(misplaced_launcher_flags(&out).is_empty());
}

#[test]
fn an_explicit_separator_still_delimits_the_tail() {
    // A caller who writes `--` themselves gets the same treatment: the
    // tail is the program's, and a launcher flag in it is discarded.
    let out = insert_program_args_separator(argv(&[
        "cratonvm",
        "-cp",
        "build",
        "--",
        "Main",
        "--trace-real-jdk",
    ]));
    assert_eq!(misplaced_launcher_flags(&out), vec!["--trace-real-jdk"]);
}

// -----------------------------------------------------------------------
// Dump-path diagnostics — the other half of G33-1 defect 2: a path the
// caller cannot find is as bad as no file at all.
// -----------------------------------------------------------------------

#[test]
fn absolute_dump_path_resolves_a_relative_operand() {
    let resolved = absolute_dump_path("reg.json");
    let path = std::path::Path::new(&resolved);
    assert!(
        path.is_absolute(),
        "a relative operand must be reported as the absolute path actually written: {resolved}"
    );
    assert!(resolved.ends_with("reg.json"));
}

#[test]
fn absolute_dump_path_never_fails_on_a_degenerate_operand() {
    // A diagnostic that can itself fail is not a diagnostic. The empty
    // operand is the one input `std::path::absolute` rejects.
    assert_eq!(absolute_dump_path(""), "");
}

#[test]
fn dump_failure_names_the_path_and_the_missing_directory() {
    let err = std::io::Error::from(std::io::ErrorKind::NotFound);
    let message = describe_dump_failure(
        "no-such-dir-4b7f1e/deeper/reg.json",
        &format!("{err} (os error 3)"),
    );
    assert!(
        message.contains("no-such-dir-4b7f1e"),
        "the attempted path must appear: {message}"
    );
    assert!(
        message.contains("does not exist"),
        "a missing parent directory is the common cause and must be named: {message}"
    );
    assert!(
        message.contains("os error 3"),
        "the underlying OS error must survive: {message}"
    );
}

#[test]
fn dump_failure_says_so_when_the_directory_does_exist() {
    // Distinguishing "no such directory" from "the directory is there and
    // the write still failed" is the whole point — the second is a
    // permissions or locking problem and sends the reader somewhere else.
    let dir = std::env::temp_dir();
    let path = dir.join("g33-1-existing-dir-probe.json");
    let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let message = describe_dump_failure(&path.display().to_string(), &err);
    assert!(
        message.contains("exists"),
        "an existing parent must be reported as existing: {message}"
    );
    assert!(!message.contains("does not exist"), "{message}");
}

#[test]
fn the_census_caveat_is_only_dropped_when_both_bypasses_are_off() {
    // Three of the four configurations leave at least one bypass family
    // live, and in all three the column is a floor. Only the fourth may
    // claim an exact count.
    for (nojit, no_intrinsics) in [(false, false), (true, false), (false, true)] {
        let text = census_invocations_caveat(nojit, no_intrinsics);
        assert!(
            text.contains("LOWER BOUND"),
            "nojit={nojit} intrinsics-off={no_intrinsics} must warn: {text}"
        );
    }
    let exact = census_invocations_caveat(true, true);
    assert!(exact.contains("exact count"), "{exact}");
    assert!(!exact.contains("LOWER BOUND"), "{exact}");
}

#[test]
fn sep_no_program_left_unchanged() {
    // `java --version` with no program: nothing to delimit; the
    // launcher must still handle `--version` itself.
    let inp = argv(&["java", "--version"]);
    assert_eq!(insert_program_args_separator(inp.clone()), inp);
    let inp = argv(&["java", "--help"]);
    assert_eq!(insert_program_args_separator(inp.clone()), inp);
}

#[test]
fn normalize_version_short_forms() {
    assert_eq!(
        normalize_java_launcher_argv(argv(&["java", "-version"])),
        argv(&["java", "--version"])
    );
    assert_eq!(
        normalize_java_launcher_argv(argv(&["java", "-v"])),
        argv(&["java", "--version"])
    );
}

#[test]
fn sep_explicit_separator_respected() {
    // An explicit `--` already delimits program args; copy verbatim.
    let inp = argv(&["java", "-jar", "a.jar", "--", "--help"]);
    assert_eq!(insert_program_args_separator(inp.clone()), inp);
}

#[test]
fn sep_value_token_not_mistaken_for_main_class() {
    // The classpath string after `-cp` is a value, not the main class.
    let out = insert_program_args_separator(argv(&["java", "-cp", "lib.jar", "Main", "arg1"]));
    assert_eq!(out, argv(&["java", "-cp", "lib.jar", "Main", "--", "arg1"]));
}

#[test]
fn sep_inline_jar_value() {
    // `--jar=foo.jar` inline form: `--version` after it is a program arg.
    let out = insert_program_args_separator(argv(&["java", "--jar=foo.jar", "--version"]));
    assert_eq!(out, argv(&["java", "--jar=foo.jar", "--", "--version"]));
}

// -----------------------------------------------------------------------
// parse_size tests
// -----------------------------------------------------------------------

#[test]
fn parse_memory_sizes() {
    assert_eq!(parse_size("256m"), Some(256 * 1024 * 1024));
    assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
    assert_eq!(parse_size("1024k"), Some(1024 * 1024));
    assert_eq!(parse_size("1024"), Some(1024));
    assert_eq!(parse_size(""), None);
}

#[test]
fn parse_size_whitespace() {
    assert_eq!(parse_size("  256m  "), Some(256 * 1024 * 1024));
    assert_eq!(parse_size("   "), None);
}

/// gc-common w6-f: `-XX:MaxMetaspaceSize=` reaches the typed limit (it was
/// dropped by the `-XX:` catch-all before), `-XX:MetaspaceSize=` is still
/// accepted and ignored, and no flag means no limit.
#[test]
fn w6f_max_metaspace_size_reaches_the_limit() {
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
        "cratonvm",
        "-XX:MetaspaceSize=16m",
        "-XX:MaxMetaspaceSize=32m",
        "-cp",
        "/tmp/cp",
        "Main",
    ])))
    .unwrap_or_else(|e| panic!("clap must accept -XX:MaxMetaspaceSize: {e}"));
    assert_eq!(parsed.max_metaspace.as_deref(), Some("32m"));
    assert_eq!(
        max_metaspace_limit(parsed.max_metaspace.as_deref()),
        Some(32 * 1024 * 1024)
    );

    let unset = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
        "cratonvm", "-cp", "/tmp/cp", "Main",
    ])))
    .expect("plain launch parses");
    assert_eq!(unset.max_metaspace, None);
    assert_eq!(max_metaspace_limit(None), None);
    assert_eq!(
        max_metaspace_limit(Some("banana")),
        None,
        "unparsable is ignored"
    );
}

#[test]
fn parse_size_invalid() {
    assert_eq!(parse_size("abc"), None);
    assert_eq!(parse_size("m"), None);
    assert_eq!(parse_size("-1m"), None);
}

#[test]
fn parse_size_overflow() {
    // Multiplying by the suffix factor must not overflow `usize`:
    // an oversized input returns None instead of panicking/wrapping.
    assert_eq!(parse_size("999999999999g"), None);
    assert_eq!(parse_size(&format!("{}g", usize::MAX)), None);
}

// -----------------------------------------------------------------------
// validate_class_name tests
// -----------------------------------------------------------------------

#[test]
fn valid_simple_class_name() {
    assert!(validate_class_name("Main").is_ok());
}

#[test]
fn valid_fully_qualified_class_name() {
    assert!(validate_class_name("com/example/Main").is_ok());
}

#[test]
fn valid_class_name_with_underscore_and_dollar() {
    assert!(validate_class_name("com/_internal/$Helper").is_ok());
    assert!(validate_class_name("$Proxy0").is_ok());
    assert!(validate_class_name("_Private").is_ok());
}

#[test]
fn valid_class_name_with_digits() {
    assert!(validate_class_name("com/example/Test123").is_ok());
}

#[test]
fn valid_class_name_with_unicode_letters() {
    // A Java identifier's letters are Unicode's (probe
    // `L5W59LauncherClassNames`).
    assert!(validate_class_name("caf\u{e9}/Caf\u{e9}$\u{dc}ber2").is_ok());
    assert!(validate_class_name("\u{43f}\u{440}\u{438}\u{43c}\u{435}\u{440}").is_ok());
}

#[test]
fn invalid_empty_class_name() {
    let err = validate_class_name("").unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn invalid_class_name_starts_with_digit() {
    let err = validate_class_name("com/123bad/Main").unwrap_err();
    assert!(err.to_string().contains("must start with"));
}

#[test]
fn invalid_class_name_double_slash() {
    let err = validate_class_name("com//Main").unwrap_err();
    assert!(err.to_string().contains("empty"));
}

#[test]
fn invalid_class_name_leading_slash() {
    let err = validate_class_name("/com/Main").unwrap_err();
    assert!(err.to_string().contains("empty"));
}

#[test]
fn invalid_class_name_trailing_slash() {
    let err = validate_class_name("com/Main/").unwrap_err();
    assert!(err.to_string().contains("empty"));
}

#[test]
fn invalid_class_name_special_chars() {
    let err = validate_class_name("com/ex@mple/Main").unwrap_err();
    assert!(err.to_string().contains("invalid character"));
}

#[test]
fn invalid_class_name_spaces() {
    let err = validate_class_name("com/my class/Main").unwrap_err();
    assert!(err.to_string().contains("invalid character"));
}

#[test]
fn invalid_class_name_hyphen() {
    let err = validate_class_name("com/my-pkg/Main").unwrap_err();
    assert!(err.to_string().contains("invalid character"));
}

// -----------------------------------------------------------------------
// extract_system_properties tests
// -----------------------------------------------------------------------

#[test]
fn extract_d_properties() {
    let raw = vec![
        "cratonvm".to_string(),
        "-Djboss.home.dir=C:/craton/kc16".to_string(),
        "-Dmy.flag".to_string(),
        "com.example.Main".to_string(),
    ];
    let (filtered, props) = extract_system_properties(raw);
    assert_eq!(filtered, vec!["cratonvm", "com.example.Main"]);
    assert_eq!(
        props,
        vec![
            ("jboss.home.dir".to_string(), "C:/craton/kc16".to_string()),
            ("my.flag".to_string(), String::new()),
        ]
    );
}

#[test]
fn extract_d_no_properties() {
    let raw = vec!["cratonvm".to_string(), "Main".to_string()];
    let (filtered, props) = extract_system_properties(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert!(props.is_empty());
}

#[test]
fn extract_d_value_with_equals() {
    // -Dkey=val=ue  →  key = "val=ue"
    let raw = vec!["cratonvm".to_string(), "-Dpath=a=b".to_string()];
    let (_, props) = extract_system_properties(raw);
    assert_eq!(props, vec![("path".to_string(), "a=b".to_string())]);
}

#[test]
fn normalize_classpath_and_jar_for_clap() {
    let raw = vec![
        "cratonvm".to_string(),
        "-classpath".to_string(),
        "a;b".to_string(),
        "-cp=c:d".to_string(),
        "-jar".to_string(),
        "app.jar".to_string(),
        "arg1".to_string(),
    ];
    assert_eq!(
        normalize_java_launcher_argv(raw),
        vec![
            "cratonvm",
            "--classpath",
            "a;b",
            "--classpath",
            "c:d",
            "--jar",
            "app.jar",
            "arg1",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
}

// -----------------------------------------------------------------------
// T6 extract_hotspot_flags tests
// -----------------------------------------------------------------------

#[test]
fn hotspot_flag_enables_heap_dump_on_oom() {
    let raw = vec![
        "cratonvm".to_string(),
        "-XX:+HeapDumpOnOutOfMemoryError".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(flags.heap_dump_on_oom, Some(true));
}

#[test]
fn hotspot_flag_disables_heap_dump_on_oom() {
    let raw = vec![
        "cratonvm".to_string(),
        "-XX:-HeapDumpOnOutOfMemoryError".to_string(),
        "Main".to_string(),
    ];
    let (_, flags) = extract_hotspot_flags(raw);
    assert_eq!(flags.heap_dump_on_oom, Some(false));
}

#[test]
fn hotspot_flag_extracts_heap_dump_path() {
    let raw = vec![
        "cratonvm".to_string(),
        "-XX:HeapDumpPath=/tmp/heap.hprof".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
}

// obsaudit D12 — -XX:StartFlightRecording extraction + option parsing.

#[test]
fn hotspot_flag_extracts_bare_start_flight_recording() {
    let raw = vec![
        "cratonvm".to_string(),
        "-XX:StartFlightRecording".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(flags.jfr_start_recording.as_deref(), Some(""));
}

#[test]
fn hotspot_flag_extracts_start_flight_recording_with_opts() {
    let raw = vec![
        "cratonvm".to_string(),
        "-XX:StartFlightRecording:filename=rec.jfr,duration=30s".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(
        flags.jfr_start_recording.as_deref(),
        Some("filename=rec.jfr,duration=30s")
    );
}

/// gcd d5/f: HotSpot's `=` spelling (`-XX:StartFlightRecording=filename=x.jfr`,
/// accepted by `java` 25 exactly like the `:` form) carries the same options.
#[test]
fn hotspot_flag_extracts_start_flight_recording_equals_form() {
    let raw = vec![
        "cratonvm".to_string(),
        "-XX:StartFlightRecording=filename=x.jfr,duration=5s".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(
        flags.jfr_start_recording.as_deref(),
        Some("filename=x.jfr,duration=5s")
    );
}

/// gcd d5/f: the launcher's own pipeline, in the order `main` runs it
/// (normaliser, `-D` extraction, HotSpot-flag extraction). All three
/// spellings used to be dropped by the normaliser's silent `-XX:`
/// catch-all before `extract_hotspot_flags` could see them; every other
/// `-XX:` token in the list keeps its old handling.
#[test]
fn start_flight_recording_survives_the_launcher_pipeline_in_every_spelling() {
    for (spelling, expected) in [
        ("-XX:StartFlightRecording", ""),
        ("-XX:StartFlightRecording:filename=a.jfr", "filename=a.jfr"),
        (
            "-XX:StartFlightRecording=filename=b.jfr,dumponexit=true",
            "filename=b.jfr,dumponexit=true",
        ),
    ] {
        let argv: Vec<String> = [
            "cratonvm",
            "-XX:+UseCompressedOops",
            spelling,
            "-XX:MetaspaceSize=64m",
            "Main",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let normalized = normalize_java_launcher_argv(argv);
        assert!(
            normalized.iter().any(|t| t == spelling),
            "{spelling}: the normaliser must keep the token, got {normalized:?}"
        );
        assert!(
            !normalized.iter().any(|t| t.starts_with("-XX:+UseCompressedOops")
                || t.starts_with("-XX:MetaspaceSize")),
            "{spelling}: the catch-all must still drop the unimplemented flags, got {normalized:?}"
        );
        let (normalized, _props) = extract_system_properties(normalized);
        let (filtered, flags) = extract_hotspot_flags(normalized);
        assert_eq!(filtered, vec!["cratonvm", "Main"], "{spelling}");
        assert_eq!(
            flags.jfr_start_recording.as_deref(),
            Some(expected),
            "{spelling}"
        );
        assert!(
            parse_jfr_start_recording_opts(expected).is_ok(),
            "{spelling}"
        );
    }
}

#[test]
fn jfr_opts_bare_flag_uses_defaults() {
    let cfg = parse_jfr_start_recording_opts("").unwrap();
    assert_eq!(cfg.filename, None);
    assert_eq!(cfg.duration, None);
    assert_eq!(cfg.max_age, None);
    assert_eq!(cfg.max_events, None);
    assert!(cfg.dump_on_exit, "HotSpot defaults dumponexit to true");
}

#[test]
fn jfr_opts_parses_all_recognized_keys() {
    let cfg = parse_jfr_start_recording_opts(
        "filename=out.jfr,duration=30s,maxage=5m,maxevents=500,dumponexit=false",
    )
    .unwrap();
    assert_eq!(cfg.filename.as_deref(), Some("out.jfr"));
    assert_eq!(cfg.duration, Some(std::time::Duration::from_secs(30)));
    assert_eq!(cfg.max_age, Some(std::time::Duration::from_secs(5 * 60)));
    assert_eq!(cfg.max_events, Some(500));
    assert!(!cfg.dump_on_exit);
}

#[test]
fn jfr_opts_bare_duration_digits_mean_seconds() {
    let cfg = parse_jfr_start_recording_opts("duration=45").unwrap();
    assert_eq!(cfg.duration, Some(std::time::Duration::from_secs(45)));
}

#[test]
fn jfr_opts_hour_and_day_suffixes() {
    let cfg = parse_jfr_start_recording_opts("maxage=2h").unwrap();
    assert_eq!(cfg.max_age, Some(std::time::Duration::from_secs(2 * 3600)));
    let cfg = parse_jfr_start_recording_opts("maxage=1d").unwrap();
    assert_eq!(cfg.max_age, Some(std::time::Duration::from_secs(86400)));
}

#[test]
fn jfr_opts_rejects_unrecognized_key() {
    let err = parse_jfr_start_recording_opts("disk=true").unwrap_err();
    assert!(err.contains("disk"), "error should name the bad key: {err}");
}

#[test]
fn jfr_opts_rejects_missing_equals() {
    let err = parse_jfr_start_recording_opts("filename").unwrap_err();
    assert!(err.contains("filename"));
}

#[test]
fn jfr_opts_rejects_bad_duration() {
    assert!(parse_jfr_start_recording_opts("duration=notanumber").is_err());
}

#[test]
fn jfr_opts_rejects_bad_dumponexit_value() {
    assert!(parse_jfr_start_recording_opts("dumponexit=maybe").is_err());
}

#[test]
fn hotspot_flag_preserves_all_agent_tokens() {
    let raw = vec![
        "cratonvm".to_string(),
        "-agentlib:jdwp=transport=dt_socket,server=y,address=5005".to_string(),
        "-agentpath:/opt/myagent.so=trace".to_string(),
        "-javaagent:/opt/bytebuddy.jar".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(flags.agent_options.len(), 3);
    assert!(flags.agent_options[0].starts_with("-agentlib:jdwp="));
    assert!(flags.agent_options[1].starts_with("-agentpath:/opt/myagent.so"));
    assert!(flags.agent_options[2].starts_with("-javaagent:/opt/bytebuddy.jar"));
}

/// Wave 85 (lane L1): `-agentlib:jdwp` options read as the JDK back end's
/// `parseOptions` reads them, with its error lines and exit statuses.
#[test]
fn jdwp_agent_options_parse_as_the_back_end_parses_them() {
    use cratonvm_vm::config::JdwpAgentConfig;
    assert_eq!(jdwp_agent_options_of("-agentlib:jdwp"), Some(None));
    assert_eq!(
        jdwp_agent_options_of("-agentlib:jdwp=help"),
        Some(Some("help"))
    );
    assert_eq!(jdwp_agent_options_of("-agentlib:jdwpx"), None);
    assert_eq!(parse_jdwp_agent_options(Some("help")), Ok(JdwpAgentParse::Help));
    assert_eq!(
        parse_jdwp_agent_options(Some("transport=dt_socket,server=y,suspend=n,address=*:5005")),
        Ok(JdwpAgentParse::Config(JdwpAgentConfig {
            server: true,
            suspend: false,
            host: Some("*".to_string()),
            port: 5005,
            quiet: false,
            timeout_ms: None,
            include_virtual_threads: false,
            allow: None,
            launch: None,
            on_throw: None,
            on_uncaught: false,
        }))
    );
    // Interpreter round i7 wave 140 (lane L1): `allow` is kept as given
    // (the transport parses it); an empty one is a syntax error.
    assert_eq!(
        parse_jdwp_agent_options(Some(
            "transport=dt_socket,server=y,address=5005,allow=127.0.0.1+10.0.0.0/8"
        )),
        Ok(JdwpAgentParse::Config(JdwpAgentConfig {
            server: true,
            suspend: true,
            host: None,
            port: 5005,
            quiet: false,
            timeout_ms: None,
            include_virtual_threads: false,
            allow: Some("127.0.0.1+10.0.0.0/8".to_string()),
            launch: None,
            on_throw: None,
            on_uncaught: false,
        }))
    );
    assert!(parse_jdwp_agent_options(Some("transport=dt_socket,server=y,allow=")).is_err());
    // Interpreter round i7 wave 144 (lane L1): `launch`, `onthrow` and
    // `onuncaught` are kept for the server; an empty value is a syntax
    // error; `onthrow` without `launch` keeps the back end's message.
    assert_eq!(
        parse_jdwp_agent_options(Some(
            "transport=dt_socket,server=y,suspend=n,address=5807,onthrow=java.lang.IllegalStateException,launch=cmd /c echo"
        )),
        Ok(JdwpAgentParse::Config(JdwpAgentConfig {
            server: true,
            suspend: false,
            host: None,
            port: 5807,
            quiet: false,
            timeout_ms: None,
            include_virtual_threads: false,
            allow: None,
            launch: Some("cmd /c echo".to_string()),
            on_throw: Some("java.lang.IllegalStateException".to_string()),
            on_uncaught: false,
        }))
    );
    assert!(parse_jdwp_agent_options(Some("transport=dt_socket,server=y,launch=")).is_err());
    assert!(parse_jdwp_agent_options(Some("transport=dt_socket,server=y,onthrow=")).is_err());
    assert!(
        parse_jdwp_agent_options(Some("transport=dt_socket,server=y,onuncaught=y")).is_err()
    );
    // Interpreter round i6 wave 130 (lane L1b): `includevirtualthreads`
    // is kept, not dropped.
    assert_eq!(
        parse_jdwp_agent_options(Some(
            "transport=dt_socket,server=y,suspend=n,address=5005,includevirtualthreads=y"
        )),
        Ok(JdwpAgentParse::Config(JdwpAgentConfig {
            server: true,
            suspend: false,
            host: None,
            port: 5005,
            quiet: false,
            timeout_ms: None,
            include_virtual_threads: true,
            allow: None,
            launch: None,
            on_throw: None,
            on_uncaught: false,
        }))
    );
    assert_eq!(
        parse_jdwp_agent_options(Some("transport=dt_socket,address=localhost:8000")),
        Ok(JdwpAgentParse::Config(JdwpAgentConfig {
            server: false,
            suspend: true,
            host: Some("localhost".to_string()),
            port: 8000,
            quiet: false,
            timeout_ms: None,
            include_virtual_threads: false,
            allow: None,
            launch: None,
            on_throw: None,
            on_uncaught: false,
        }))
    );
    let lines = |options: Option<&str>| {
        let failure = parse_jdwp_agent_options(options).unwrap_err();
        (failure.stderr_lines, failure.exit_code)
    };
    assert_eq!(
        lines(Some("transport=dt_socket,server=yes")),
        (
            vec![
                "ERROR: JDWP option syntax error: -agentlib:jdwp=transport=dt_socket,server=yes"
                    .to_string()
            ],
            1
        )
    );
    assert_eq!(
        lines(None),
        (
            vec!["ERROR: JDWP no transport specified: -agentlib:jdwp=".to_string()],
            1
        )
    );
    assert_eq!(
        lines(Some("address=5005,transport=dt_socket")).0,
        vec![
            "ERROR: JDWP address specified without transport: -agentlib:jdwp=address=5005,transport=dt_socket"
                .to_string()
        ]
    );
    assert_eq!(
        lines(Some("transport=dt_socket")),
        (
            vec![
                "ERROR: JDWP Non-server transport dt_socket must have a connection address specified through the 'address=' option".to_string(),
                "ERROR: JDWP invalid option: -agentlib:jdwp=transport=dt_socket".to_string(),
            ],
            1
        )
    );
    assert_eq!(lines(Some("transport=dt_shmem,server=y")).1, 2);
    assert_eq!(
        lines(Some("transport=dt_socket,server=y,address=x:99999")).0[0],
        "ERROR: transport error 103: invalid port number specified"
    );
}

/// Wave 84 (lane L1): `java.rt.vmArgs` as HotSpot's `jvm_args()` holds
/// it -- VM options in order, module options joined with `=`, the class
/// path, launcher-only options and program arguments left out.
#[test]
fn hotspot_jvm_args_are_the_vm_options() {
    let argv: Vec<String> = [
        "cratonvm",
        "--real-jdk",
        "-Xmx64m",
        "-cp",
        "/a:/b",
        "-Dx=y",
        "--add-opens",
        "java.base/java.lang=ALL-UNNAMED",
        "-ea:com.foo...",
        "-verbose:gc",
        "-XX:+UseSerialGC",
        "-XshowSettings:vm",
        "-Djava.class.path=/c",
        "-javaagent:a.jar=k",
        "-p",
        "mods",
        "Main",
        "--",
        "-Xprogram",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        hotspot_jvm_args(&argv),
        [
            "-Xmx64m",
            "-Dx=y",
            "--add-opens=java.base/java.lang=ALL-UNNAMED",
            "-ea:com.foo...",
            "-verbose:gc",
            "-XX:+UseSerialGC",
            "-javaagent:a.jar=k",
            "--module-path=mods",
        ]
    );
}

/// JPLIS's `Agent_OnLoad` lines for an unusable `-javaagent:` JAR, the
/// JAR spelled as given (interpreter round i4 wave 84, lane L1).
#[test]
fn javaagent_jar_failure_lines_are_jplis_lines() {
    let missing = cratonvm_vm::runtime::agent_loader::parse_javaagent_spec(
        "-javaagent:no-such-dir-l1w84/missing.jar=opts",
    )
    .expect_err("a missing JAR is refused");
    assert_eq!(
        javaagent_jar_failure_line(&missing).as_deref(),
        Some("Error opening zip file or JAR manifest missing : no-such-dir-l1w84/missing.jar")
    );
    let no_premain = cratonvm_vm::runtime::agent_loader::AgentLoadError::MissingPremainClass {
        jar: std::path::PathBuf::from("a.jar"),
    };
    assert_eq!(
        javaagent_jar_failure_line(&no_premain).as_deref(),
        Some("Failed to find Premain-Class manifest attribute in a.jar")
    );
}

#[test]
fn hotspot_flag_passes_unknown_through() {
    let raw = vec![
        "cratonvm".to_string(),
        "--foo".to_string(),
        "Main".to_string(),
    ];
    let (filtered, flags) = extract_hotspot_flags(raw);
    assert_eq!(filtered, vec!["cratonvm", "--foo", "Main"]);
    assert!(flags.agent_options.is_empty());
    assert!(flags.heap_dump_on_oom.is_none());
    assert!(flags.heap_dump_path.is_none());
}

#[test]
fn heap_dump_flags_survive_full_launcher_pipeline() {
    let argv0 = argv(&[
        "java",
        "-XX:+HeapDumpOnOutOfMemoryError",
        "-XX:HeapDumpPath=/tmp/heap.hprof",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept heap dump flags");

    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.heap_dump_on_oom, Some(true));
    assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
}

/// gc-common w8-g: the explicit-GC flags survive the whole launcher
/// pipeline (they used to be dropped by the `-XX:` catch-all), clap never
/// sees them, and they select the per-VM policy with HotSpot's precedence.
#[test]
fn w8g_explicit_gc_flags_survive_full_launcher_pipeline() {
    use cratonvm_vm::config::ExplicitGcPolicy;
    let run = |flags: &[&str]| {
        let mut v = vec!["java"];
        v.extend_from_slice(flags);
        v.push("Main");
        let stage1 = insert_program_args_separator(argv(&v));
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, hot) = extract_hotspot_flags(stage3);
        assert!(
            !stage4.iter().any(|t| t.contains("ExplicitGC")),
            "an explicit-GC flag reached clap: {stage4:?}"
        );
        let parsed = Args::try_parse_from(stage4).expect("clap accepts the rest");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        ExplicitGcPolicy::from_hotspot_flags(
            hot.disable_explicit_gc,
            hot.explicit_gc_invokes_concurrent,
        )
    };
    assert_eq!(run(&[]), ExplicitGcPolicy::Full);
    assert_eq!(run(&["-XX:+DisableExplicitGC"]), ExplicitGcPolicy::Disabled);
    assert_eq!(
        run(&["-XX:+ExplicitGCInvokesConcurrent"]),
        ExplicitGcPolicy::Concurrent
    );
    assert_eq!(
        run(&["-XX:+ExplicitGCInvokesConcurrent", "-XX:+DisableExplicitGC"]),
        ExplicitGcPolicy::Disabled,
        "DisableExplicitGC wins"
    );
    assert_eq!(
        run(&["-XX:+DisableExplicitGC", "-XX:-DisableExplicitGC"]),
        ExplicitGcPolicy::Full,
        "last one wins"
    );
    assert_eq!(
        run(&["-XX:-ExplicitGCInvokesConcurrent"]),
        ExplicitGcPolicy::Full
    );
}

/// gen r4w6/young6: the HotSpot tenuring flags survive the whole launcher
/// pipeline (they used to be dropped by the normaliser's silent `-XX:`
/// catch-all), clap never sees them, the last value wins, and (gci
/// i23/tn) a bad value is HotSpot's refusal, not a warning.
#[test]
fn young6_tenuring_flags_survive_full_launcher_pipeline() {
    let run = |flags: &[&str]| {
        let mut v = vec!["java"];
        v.extend_from_slice(flags);
        v.push("Main");
        let stage1 = insert_program_args_separator(argv(&v));
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, hot) = extract_hotspot_flags(stage3);
        assert!(
            !stage4
                .iter()
                .any(|t| t.contains("Tenuring") || t.contains("SurvivorRatio")),
            "a tenuring flag reached clap: {stage4:?}"
        );
        let parsed = Args::try_parse_from(stage4).expect("clap accepts the rest");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        tenuring_config_from_flags(&hot)
    };

    let cfg = run(&[]).expect("nothing typed");
    assert!(cfg.is_default());

    let cfg = run(&[
        "-XX:MaxTenuringThreshold=4",
        "-XX:MaxTenuringThreshold=6",
        "-XX:InitialTenuringThreshold=3",
        "-XX:TargetSurvivorRatio=90",
        "-XX:+PrintTenuringDistribution",
    ])
    .expect("all in range");
    assert_eq!(cfg.max_tenuring_threshold, Some(6), "last one wins");
    assert_eq!(cfg.initial_tenuring_threshold, Some(3));
    assert_eq!(cfg.target_survivor_ratio, Some(90));
    assert!(cfg.print_tenuring_distribution);
    assert!(cfg.requests_adaptive());

    // gci i23/tn: 16 is HotSpot's `NeverTenure`, inside the range.
    let cfg = run(&["-XX:MaxTenuringThreshold=16"]).expect("16 is in range");
    assert_eq!(cfg.max_tenuring_threshold, Some(16));

    let lines = run(&["-XX:MaxTenuringThreshold=17", "-XX:TargetSurvivorRatio=x"])
        .expect_err("HotSpot refuses to start");
    assert_eq!(
        lines,
        vec!["uint MaxTenuringThreshold=17 is outside the allowed range [ 0 ... 16 ]".to_string()],
        "the first bad option, MaxTenuringThreshold's own parser: no Improperly line"
    );

    let cfg = run(&[
        "-XX:+PrintTenuringDistribution",
        "-XX:-PrintTenuringDistribution",
    ])
    .expect("toggles only");
    assert!(!cfg.print_tenuring_distribution, "last toggle wins");
}

/// gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`
/// item 5): the HotSpot 25 refusal lines of the three tenuring values, as
/// `Arguments::parse_each_vm_init_arg` (`MaxTenuringThreshold`) and
/// `Arguments::process_argument` (the other two) print them.
#[test]
fn gci_i23_tn_tenuring_values_are_refused_as_hotspot_refuses_them() {
    let flags = |max: Option<&str>, initial: Option<&str>, ratio: Option<&str>| HotspotFlags {
        max_tenuring_threshold: max.map(str::to_string),
        initial_tenuring_threshold: initial.map(str::to_string),
        target_survivor_ratio: ratio.map(str::to_string),
        ..HotspotFlags::default()
    };
    let ok = |f: HotspotFlags| tenuring_config_from_flags(&f).expect("accepted");
    let err = |f: HotspotFlags| tenuring_config_from_flags(&f).expect_err("refused");

    // The ranges' ends are accepted.
    let cfg = ok(flags(Some("0"), Some("16"), Some("100")));
    assert_eq!(cfg.max_tenuring_threshold, Some(0));
    assert_eq!(cfg.initial_tenuring_threshold, Some(16));
    assert_eq!(cfg.target_survivor_ratio, Some(100));
    assert_eq!(ok(flags(Some("0x5"), None, Some("0"))).max_tenuring_threshold, Some(5));

    // MaxTenuringThreshold: its own parser.
    assert_eq!(
        err(flags(Some("x"), None, None)),
        vec!["Improperly specified VM option 'MaxTenuringThreshold=x'".to_string()]
    );
    assert_eq!(
        err(flags(Some("-1"), None, None)),
        vec!["Improperly specified VM option 'MaxTenuringThreshold=-1'".to_string()]
    );
    assert_eq!(
        err(flags(Some("1k"), None, None)),
        vec!["uint MaxTenuringThreshold=1024 is outside the allowed range [ 0 ... 16 ]".to_string()]
    );

    // The other two: the range line, then the Improperly line.
    assert_eq!(
        err(flags(None, Some("17"), None)),
        vec![
            "uint InitialTenuringThreshold=17 is outside the allowed range [ 0 ... 16 ]".to_string(),
            "Improperly specified VM option 'InitialTenuringThreshold=17'".to_string(),
        ]
    );
    assert_eq!(
        err(flags(None, None, Some("101"))),
        vec![
            "uint TargetSurvivorRatio=101 is outside the allowed range [ 0 ... 100 ]".to_string(),
            "Improperly specified VM option 'TargetSurvivorRatio=101'".to_string(),
        ]
    );
    assert_eq!(
        err(flags(None, None, Some("ninety"))),
        vec!["Improperly specified VM option 'TargetSurvivorRatio=ninety'".to_string()]
    );

    // The number parser.
    assert_eq!(hotspot_parse_uintx("15"), Some(15));
    assert_eq!(hotspot_parse_uintx(" 7 "), None, "gci i48/rv48: no trim");
    assert_eq!(hotspot_parse_uintx("0X10"), Some(16));
    assert_eq!(hotspot_parse_uintx("2m"), Some(2 << 20));
    assert_eq!(hotspot_parse_uintx("+5"), None);
    assert_eq!(hotspot_parse_uintx(""), None);
    assert_eq!(hotspot_parse_uintx("5x"), None);
    assert_eq!(hotspot_parse_uintx("99999999999999999999"), None);
    assert_eq!(hotspot_parse_uintx("16777216T"), None, "overflow");
}

/// gci i27/gs (`gci-i23tn-...` item 3): every tenuring value is checked
/// in command-line order and the first bad one refuses, as HotSpot's
/// option loop does; an accepted set still takes the last value of each.
#[test]
fn gci_i27_gs_tenuring_values_are_checked_in_command_line_order() {
    let parse = |args: &[&str]| {
        let raw: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (_rest, hot) = extract_hotspot_flags(raw);
        tenuring_config_from_flags(&hot)
    };
    // A bad value later overridden by a good one still refuses.
    assert_eq!(
        parse(&["-XX:MaxTenuringThreshold=17", "-XX:MaxTenuringThreshold=5"])
            .expect_err("HotSpot stops at the first"),
        vec!["uint MaxTenuringThreshold=17 is outside the allowed range [ 0 ... 16 ]".to_string()]
    );
    // Two different bad flags: the EARLIER one is reported.
    assert_eq!(
        parse(&["-XX:TargetSurvivorRatio=x", "-XX:MaxTenuringThreshold=17"])
            .expect_err("refused"),
        vec!["Improperly specified VM option 'TargetSurvivorRatio=x'".to_string()]
    );
    assert_eq!(
        parse(&["-XX:InitialTenuringThreshold=20", "-XX:TargetSurvivorRatio=101"])
            .expect_err("refused"),
        vec![
            "uint InitialTenuringThreshold=20 is outside the allowed range [ 0 ... 16 ]".to_string(),
            "Improperly specified VM option 'InitialTenuringThreshold=20'".to_string(),
        ]
    );
    // All good: the last of each wins, as before.
    let cfg = parse(&[
        "-XX:MaxTenuringThreshold=3",
        "-XX:TargetSurvivorRatio=10",
        "-XX:MaxTenuringThreshold=16",
    ])
    .expect("all in range");
    assert_eq!(cfg.max_tenuring_threshold, Some(16));
    assert_eq!(cfg.target_survivor_ratio, Some(10));
    // Hand-built flags (no order) keep the old per-flag check.
    let hand = HotspotFlags {
        max_tenuring_threshold: Some("4".to_string()),
        ..HotspotFlags::default()
    };
    assert_eq!(tenuring_config_from_flags(&hand).expect("ok").max_tenuring_threshold, Some(4));    }

/// gci i48/rv48 (`gci-i24rv-...` item 14b): a blank around the value is
/// HotSpot 25's `parse_integer` refusal, `Improperly specified` with the
/// value as typed, on both parsers' flags (it used to be trimmed and
/// accepted).
#[test]
fn gci_i48_rv48_a_blank_around_the_value_is_improperly_specified() {
    let hot = |args: &[&str]| {
        let raw: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        extract_hotspot_flags(raw).1
    };
    assert_eq!(hotspot_parse_uintx(" 5"), None);
    assert_eq!(hotspot_parse_uintx("5 "), None);
    assert_eq!(hotspot_parse_uintx("5"), Some(5));
    assert_eq!(
        tenuring_config_from_flags(&hot(&["-XX:MaxTenuringThreshold= 5"]))
            .expect_err("blank"),
        vec!["Improperly specified VM option 'MaxTenuringThreshold= 5'".to_string()]
    );
    assert_eq!(
        tenuring_config_from_flags(&hot(&["-XX:TargetSurvivorRatio=50 "]))
            .expect_err("blank"),
        vec!["Improperly specified VM option 'TargetSurvivorRatio=50 '".to_string()]
    );
    assert_eq!(
        space_tlab_flags_from_hotspot_flags(&hot(&["-XX:SurvivorRatio= 8"]))
            .expect_err("blank"),
        vec!["Improperly specified VM option 'SurvivorRatio= 8'".to_string()]
    );
}

/// gci i47w/ob47 (`vi27-gs.list` TO1 on HotSpot 25.0.3, rows
/// `gs27_to_repeat_bad_first`, `gs27_to_itt_then_tsr`, `gs27_to_cpt_repeat`):
/// HotSpot 25 declares `MaxTenuringThreshold`, `InitialTenuringThreshold`
/// and `TargetSurvivorRatio` as `uint`, and its range line names the
/// declared type -- `uint MaxTenuringThreshold=17 is outside ...`, not
/// `uintx`. The `Improperly specified` companion line is unchanged.
#[test]
fn gci_i47w_ob47_tenuring_range_lines_name_hotspots_uint_type() {
    let parse = |args: &[&str]| {
        let raw: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (_rest, hot) = extract_hotspot_flags(raw);
        tenuring_config_from_flags(&hot)
    };
    let cases: [(&str, Vec<&str>); 3] = [
        (
            "-XX:MaxTenuringThreshold=17",
            vec!["uint MaxTenuringThreshold=17 is outside the allowed range [ 0 ... 16 ]"],
        ),
        (
            "-XX:InitialTenuringThreshold=20",
            vec![
                "uint InitialTenuringThreshold=20 is outside the allowed range [ 0 ... 16 ]",
                "Improperly specified VM option 'InitialTenuringThreshold=20'",
            ],
        ),
        (
            "-XX:TargetSurvivorRatio=101",
            vec![
                "uint TargetSurvivorRatio=101 is outside the allowed range [ 0 ... 100 ]",
                "Improperly specified VM option 'TargetSurvivorRatio=101'",
            ],
        ),
    ];
    for (arg, want) in cases {
        let got = parse(&[arg]).expect_err("HotSpot refuses to start");
        let want: Vec<String> = want.into_iter().map(str::to_string).collect();
        assert_eq!(got, want, "{arg}");
        assert!(!got[0].starts_with("uintx "), "{arg}: HotSpot 25 says `uint`");
    }
}

/// gci i43/lf (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`):
/// `-XX:SurvivorRatio` and the TLAB flags survive the whole launcher
/// pipeline (the `-XX:` catch-all dropped them), clap never sees them,
/// and a legal set is carried with the last value of each.
#[test]
fn gci_i43_lf_space_tlab_flags_survive_full_launcher_pipeline() {
    let run = |flags: &[&str]| {
        let mut v = vec!["java"];
        v.extend_from_slice(flags);
        v.push("Main");
        let stage1 = insert_program_args_separator(argv(&v));
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, hot) = extract_hotspot_flags(stage3);
        assert!(
            !stage4.iter().any(|t| t.contains("TLAB") || t.contains("SurvivorRatio")),
            "a space / TLAB flag reached clap: {stage4:?}"
        );
        let parsed = Args::try_parse_from(stage4).expect("clap accepts the rest");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        space_tlab_flags_from_hotspot_flags(&hot)
    };
    let (sr, tlab) = run(&[]).expect("nothing given");
    assert_eq!(sr, None);
    assert!(tlab.is_default());
    let (sr, tlab) = run(&[
        "-XX:SurvivorRatio=6",
        "-XX:SurvivorRatio=4",
        "-XX:TLABSize=64k",
        "-XX:MinTLABSize=4096",
        "-XX:-UseTLAB",
        "-XX:-ResizeTLAB",
        "-XX:+ResizeTLAB",
        "-XX:TLABWasteTargetPercent=10",
        "-XX:TLABRefillWasteFraction=0x10",
        "-XX:TLABAllocationWeight=0",
        "-XX:TLABWasteIncrement=8",
    ])
    .expect("all legal");
    assert_eq!(sr, Some(4));
    assert_eq!(tlab.tlab_size, Some(64 * 1024));
    assert_eq!(tlab.min_tlab_size, Some(4096));
    assert_eq!(tlab.use_tlab, Some(false));
    assert_eq!(tlab.resize_tlab, Some(true));
    assert_eq!(tlab.waste_target_percent, Some(10));
    assert_eq!(tlab.refill_waste_fraction, Some(16));
    assert_eq!(tlab.allocation_weight, Some(0));
    assert_eq!(tlab.waste_increment, Some(8));
    // `TargetSurvivorRatio` is still the tenuring flag, not this one.
    assert_eq!(space_tlab_value_flag("-XX:TargetSurvivorRatio=50"), None);
    assert!(!is_space_tlab_flag("-XX:TargetSurvivorRatio=50"));
    assert!(is_space_tlab_flag("-XX:SurvivorRatio=8"));
}

/// gci i43/lf: HotSpot 25's parse-time refusals -- the range line plus
/// `Improperly specified` for a number out of range, the `Improperly
/// specified` line alone for text -- and the first bad occurrence in
/// command-line order wins.
#[test]
fn gci_i43_lf_space_tlab_values_are_refused_as_hotspot_refuses_them() {
    let parse = |args: &[&str]| {
        let raw: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (_rest, hot) = extract_hotspot_flags(raw);
        space_tlab_flags_from_hotspot_flags(&hot)
    };
    let err = |args: &[&str]| parse(args).expect_err("refused");
    let pair = |range: &str, improper: &str| vec![range.to_string(), improper.to_string()];
    assert_eq!(
        err(&["-XX:SurvivorRatio=0"]),
        pair(
            "uintx SurvivorRatio=0 is outside the allowed range [ 1 ... 18446744073709551613 ]",
            "Improperly specified VM option 'SurvivorRatio=0'"
        )
    );
    assert_eq!(
        err(&["-XX:TLABWasteTargetPercent=0"]),
        pair(
            "uintx TLABWasteTargetPercent=0 is outside the allowed range [ 1 ... 100 ]",
            "Improperly specified VM option 'TLABWasteTargetPercent=0'"
        )
    );
    assert_eq!(
        err(&["-XX:TLABWasteTargetPercent=101"]),
        pair(
            "uintx TLABWasteTargetPercent=101 is outside the allowed range [ 1 ... 100 ]",
            "Improperly specified VM option 'TLABWasteTargetPercent=101'"
        )
    );
    assert_eq!(
        err(&["-XX:TLABRefillWasteFraction=0"]),
        pair(
            "uintx TLABRefillWasteFraction=0 is outside the allowed range [ 1 ... 4294967295 ]",
            "Improperly specified VM option 'TLABRefillWasteFraction=0'"
        )
    );
    assert_eq!(
        err(&["-XX:TLABAllocationWeight=101"]),
        pair(
            "uintx TLABAllocationWeight=101 is outside the allowed range [ 0 ... 100 ]",
            "Improperly specified VM option 'TLABAllocationWeight=101'"
        )
    );
    assert_eq!(
        err(&["-XX:MinTLABSize=0"]),
        pair(
            "size_t MinTLABSize=0 is outside the allowed range [ 1 ... 9223372036854775807 ]",
            "Improperly specified VM option 'MinTLABSize=0'"
        )
    );
    assert_eq!(
        err(&["-XX:TLABWasteIncrement=2147483648"]),
        pair(
            "uintx TLABWasteIncrement=2147483648 is outside the allowed range [ 0 ... 2147483647 ]",
            "Improperly specified VM option 'TLABWasteIncrement=2147483648'"
        )
    );
    assert_eq!(
        err(&["-XX:TLABSize=-1"]),
        vec!["Improperly specified VM option 'TLABSize=-1'".to_string()]
    );
    assert_eq!(
        err(&["-XX:SurvivorRatio=eight"]),
        vec!["Improperly specified VM option 'SurvivorRatio=eight'".to_string()]
    );
    // The first bad occurrence, even when a later one is legal.
    assert_eq!(
        err(&["-XX:TLABAllocationWeight=x", "-XX:SurvivorRatio=0", "-XX:TLABAllocationWeight=5"]),
        vec!["Improperly specified VM option 'TLABAllocationWeight=x'".to_string()]
    );
    // `TLABSize=1` passes the parse (no range); its constraint refuses it.
    let (sr, tlab) = parse(&["-XX:TLABSize=1"]).expect("no range on TLABSize");
    assert_eq!(sr, None);
    assert_eq!(tlab.tlab_size, Some(1));
}

/// gci i43/lf: HotSpot 25's `AfterMemoryInit` constraints
/// (`jvmFlagConstraintsGC.cpp`) on Serial and G1 geometry; every failure
/// is reported, in flag-table order.
#[test]
fn gci_i43_lf_space_tlab_constraints_match_hotspot() {
    use cratonvm_vm::config::{GcAlgorithm, TlabFlags};
    const M: usize = 1024 * 1024;
    // Serial at -Xmx64m: SpaceAlignment 64K, so SurvivorRatio <= 1024;
    // the TLAB maximum is the int[max_jint] filler, 8 589 934 576 bytes.
    let serial = hotspot_space_tlab_geometry(GcAlgorithm::Generational, 64 * M, None);
    assert_eq!(serial, Some((64 * 1024, 64 * M as u64, 8_589_934_576)));
    // -Xmx9m aligns up to 10M (2M HeapAlignment): 160.
    assert_eq!(
        hotspot_space_tlab_geometry(GcAlgorithm::Generational, 9 * M, None).map(|g| g.1),
        Some(10 * M as u64)
    );
    // G1 at -Xmx64m: 1M regions, half a region of TLAB; at -Xmx16g 8M.
    let g1 = hotspot_space_tlab_geometry(GcAlgorithm::G1, 64 * M, None);
    assert_eq!(g1, Some((M as u64, 64 * M as u64, (M / 2) as u64)));
    assert_eq!(
        hotspot_space_tlab_geometry(GcAlgorithm::G1, 16384 * M, None).map(|g| g.0),
        Some(8 * M as u64)
    );
    // An explicit region size wins, rounded up to a power of two.
    assert_eq!(
        hotspot_space_tlab_geometry(GcAlgorithm::G1, 64 * M, Some(3 * M)).map(|g| g.0),
        Some(4 * M as u64)
    );
    // gci zl1 (zx48 A8): ZGC at -Xmx9m: 2M granules (SurvivorRatio <= 5
    // of a 10M heap), `ZObjectSizeLimitSmall` = 256K of TLAB.
    let zgc = hotspot_space_tlab_geometry(GcAlgorithm::Zgc, 9 * M, None);
    assert_eq!(zgc, Some((2 * M as u64, 10 * M as u64, 256 * 1024)));
    let min300k = TlabFlags { min_tlab_size: Some(300 * 1024), ..TlabFlags::default() };
    assert_eq!(
        space_tlab_constraint_errors(Some(6), &min300k, zgc),
        vec![
            concat!(
                "SurvivorRatio (6) must be less than or equal to ",
                "ergonomic SurvivorRatio maximum (5)"
            )
            .to_string(),
            concat!(
                "MinTLABSize (307200) must be less than or equal to ",
                "ergonomic TLAB maximum (262144)"
            )
            .to_string(),
        ]
    );

    let none = TlabFlags::default();
    assert!(space_tlab_constraint_errors(None, &none, serial).is_empty());
    assert!(space_tlab_constraint_errors(Some(1024), &none, serial).is_empty());
    assert_eq!(
        space_tlab_constraint_errors(Some(1025), &none, serial),
        vec![
            "SurvivorRatio (1025) must be less than or equal to ergonomic SurvivorRatio maximum (1024)"
                .to_string()
        ]
    );
    assert_eq!(
        space_tlab_constraint_errors(Some(65), &none, g1),
        vec![
            "SurvivorRatio (65) must be less than or equal to ergonomic SurvivorRatio maximum (64)"
                .to_string()
        ]
    );
    let ts1 = TlabFlags { tlab_size: Some(1), ..TlabFlags::default() };
    assert_eq!(
        space_tlab_constraint_errors(None, &ts1, serial),
        vec!["TLABSize (1) must be greater than or equal to MinTLABSize (2048)".to_string()]
    );
    let big = TlabFlags {
        tlab_size: Some(M as u64),
        min_tlab_size: Some(M as u64),
        ..TlabFlags::default()
    };
    assert!(space_tlab_constraint_errors(None, &big, serial).is_empty());
    assert_eq!(
        space_tlab_constraint_errors(Some(100), &big, g1),
        vec![
            "SurvivorRatio (100) must be less than or equal to ergonomic SurvivorRatio maximum (64)"
                .to_string(),
            "MinTLABSize (1048576) must be less than or equal to ergonomic TLAB maximum (524288)"
                .to_string(),
            "TLABSize (1048576) must be less than or equal to ergonomic TLAB maximum size (524288)"
                .to_string(),
        ]
    );
    // No geometry (no collector answers `None` since gci zl1; the guard
    // stays): nothing refused.
    assert!(space_tlab_constraint_errors(Some(u64::MAX - 2), &ts1, None).is_empty());
}

/// gci i47w/j43: a command-line `MinTLABSize` raises G1's two PLAB sizes
/// ergonomically to `PLAB::min_size()`, and HotSpot 25 refuses one above
/// the TLAB maximum in words after the TLAB lines (HotSpot 25.0.3,
/// `-XX:+UseG1GC -Xmx64m -XX:SurvivorRatio=100 -XX:MinTLABSize=1m
/// -XX:TLABSize=1m`, row `lf43_r_three_g1`).
#[test]
fn gci_i47w_j43_g1_plab_lines_follow_the_tlab_lines() {
    const M: u64 = 1024 * 1024;
    let half_region = M / 2;
    assert!(g1_plab_constraint_errors(None, half_region).is_empty());
    assert!(g1_plab_constraint_errors(Some(4096), half_region).is_empty());
    // 512K / 8 + 2 = 65 538 words > 65 536: the boundary value refuses.
    assert!(g1_plab_constraint_errors(Some(half_region - 16), half_region).is_empty());
    assert_eq!(g1_plab_constraint_errors(Some(half_region), half_region).len(), 2);
    assert_eq!(
        g1_plab_constraint_errors(Some(M), half_region),
        vec![
            "YoungPLABSize (131074) must be less than or equal to ergonomic PLAB maximum size (65536)"
                .to_string(),
            "OldPLABSize (131074) must be less than or equal to ergonomic PLAB maximum size (65536)"
                .to_string(),
        ]
    );
    // The launcher appends them, on G1 only (HotSpot's default G1 when no
    // collector is named), after the TLAB lines.
    let src = include_str!("main_parts/arch_label.rs").replace("\r\n", "\n");
    assert!(src.contains(concat!(
        "            let mut lines = space_tlab_constraint_errors(sr, &tlab, geometry);\n",
        "            if matches!(constraint_gc, cratonvm_vm::config::GcAlgorithm::G1) {\n",
        "                if let Some((_, _, tlab_max)) = geometry {\n",
        "                    lines.extend(g1_plab_constraint_errors(tlab.min_tlab_size, tlab_max));\n",
    )));
}

/// gen r4w5/thrash5: the three other `report_java_out_of_memory` options
/// survive the whole launcher pipeline (they used to be dropped by the
/// normaliser's silent `-XX:` catch-all), a command with spaces and `%p`
/// stays one token, and repeated `OnOutOfMemoryError` options accumulate.
#[test]
fn oom_report_flags_survive_full_launcher_pipeline() {
    let argv0 = argv(&[
        "java",
        "-XX:+ExitOnOutOfMemoryError",
        "-XX:+CrashOnOutOfMemoryError",
        "-XX:OnOutOfMemoryError=echo oom-hook %p",
        "-XX:OnOutOfMemoryError=echo second",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the OOM report flags");

    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.exit_on_oom, Some(true));
    assert_eq!(flags.crash_on_oom, Some(true));
    assert_eq!(
        flags.on_oom_error.as_deref(),
        Some("echo oom-hook %p\necho second")
    );
}

/// gen r4w5/thrash5: the `-` spellings turn the switches off, and none of
/// the three is set when absent (the config keeps its defaults).
#[test]
fn oom_report_flags_minus_spellings_and_absence() {
    let (filtered, flags) = extract_hotspot_flags(vec![
        "cratonvm".to_string(),
        "-XX:-ExitOnOutOfMemoryError".to_string(),
        "-XX:-CrashOnOutOfMemoryError".to_string(),
        "Main".to_string(),
    ]);
    assert_eq!(filtered, vec!["cratonvm", "Main"]);
    assert_eq!(flags.exit_on_oom, Some(false));
    assert_eq!(flags.crash_on_oom, Some(false));
    assert!(flags.on_oom_error.is_none());

    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.exit_on_oom.is_none());
    assert!(none.crash_on_oom.is_none());
    assert!(none.on_oom_error.is_none());
}

/// gcf f2/o: `-XX:±UseGCOverheadLimit` survives the normaliser (it used to
/// fall into the silent `-XX:` catch-all), the last spelling wins, and
/// absence leaves `None`.
#[test]
fn gcf_f2o_use_gc_overhead_limit_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:+UseGCOverheadLimit",
        "-XX:-UseGCOverheadLimit",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:-UseGCOverheadLimit"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept -XX:±UseGCOverheadLimit");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.use_gc_overhead_limit, Some(false));

    let (_, plus) = extract_hotspot_flags(vec![
        "cratonvm".to_string(),
        "-XX:+UseGCOverheadLimit".to_string(),
        "Main".to_string(),
    ]);
    assert_eq!(plus.use_gc_overhead_limit, Some(true));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.use_gc_overhead_limit.is_none());
}

/// Interpreter round i2 wave 59, lane L1: `-XX:±DisableAttachMechanism`
/// survives the normaliser (it used to fall into the silent `-XX:`
/// catch-all), the last spelling wins, and absence leaves `None`.
#[test]
fn i59_disable_attach_mechanism_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:-DisableAttachMechanism",
        "-XX:+DisableAttachMechanism",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:+DisableAttachMechanism"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept -XX:±DisableAttachMechanism");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.disable_attach_mechanism, Some(true));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.disable_attach_mechanism.is_none());
}

#[test]
fn i59_perf_flags_survive_normalization_and_last_spelling_wins() {
    let args = argv(&[
        "java",
        "-XX:-UsePerfData",
        "-XX:+UsePerfData",
        "-XX:+PerfDisableSharedMem",
        "-XX:-PerfDisableSharedMem",
        "Main",
    ]);
    let normalized = normalize_java_launcher_argv(insert_program_args_separator(args));
    let (remaining, flags) = extract_hotspot_flags(normalized);
    assert_eq!(flags.use_perf_data, Some(true));
    assert_eq!(flags.perf_disable_shared_mem, Some(false));
    assert_eq!(
        Args::try_parse_from(remaining)
            .unwrap()
            .class_name
            .as_deref(),
        Some("Main")
    );
}

/// nio21 wave 16, lane L5: `-XX:±OmitStackTraceInFastThrow` survives the
/// normaliser (it used to fall into the silent `-XX:` catch-all), the last
/// spelling wins, and absence leaves `None`.
#[test]
fn nio21_w16_l5_omit_stack_trace_in_fast_throw_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:+OmitStackTraceInFastThrow",
        "-XX:-OmitStackTraceInFastThrow",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:-OmitStackTraceInFastThrow"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept -XX:±OmitStackTraceInFastThrow");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.omit_stack_trace_in_fast_throw, Some(false));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.omit_stack_trace_in_fast_throw.is_none());
}

/// Interpreter round i4 wave 84, lane L2: `-XX:±ShowHiddenFrames` survives
/// the normaliser (it used to fall into the silent `-XX:` catch-all), the
/// last spelling wins, and absence leaves `None` (HotSpot's off).
#[test]
fn i84_l2_show_hidden_frames_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:-ShowHiddenFrames",
        "-XX:+ShowHiddenFrames",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:+ShowHiddenFrames"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:±ShowHiddenFrames");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.show_hidden_frames, Some(true));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.show_hidden_frames.is_none());
}

/// Interpreter round i5 wave 102, lane L6:
/// `-XX:DiagnoseSyncOnValueBasedClasses=<n>` survives the normaliser, the
/// last one wins, and a bad value answers HotSpot 25's refusal lines.
#[test]
fn i102_l6_diagnose_sync_on_value_based_classes_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:+UnlockDiagnosticVMOptions",
        "-XX:DiagnoseSyncOnValueBasedClasses=1",
        "-XX:DiagnoseSyncOnValueBasedClasses=2",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2
            .iter()
            .any(|a| a == "-XX:DiagnoseSyncOnValueBasedClasses=2"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4)
        .expect("clap must accept -XX:DiagnoseSyncOnValueBasedClasses");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(
        flags.diagnose_sync_on_value_based_classes.as_deref(),
        Some("2")
    );
    assert_eq!(diagnose_sync_on_value_based_classes_from_flag("0"), Ok(0));
    assert_eq!(diagnose_sync_on_value_based_classes_from_flag("2"), Ok(2));
    assert_eq!(
        diagnose_sync_on_value_based_classes_from_flag("3"),
        Err(vec![
            "int DiagnoseSyncOnValueBasedClasses=3 is outside the allowed range [ 0 ... 2 ]"
                .to_string(),
            "Improperly specified VM option 'DiagnoseSyncOnValueBasedClasses=3'".to_string(),
        ])
    );
    assert_eq!(
        diagnose_sync_on_value_based_classes_from_flag("x"),
        Err(vec![
            "Improperly specified VM option 'DiagnoseSyncOnValueBasedClasses=x'".to_string()
        ])
    );
}

/// Interpreter round i4 wave 85, lane L2: `-XX:±StackTraceInThrowable`
/// survives the normaliser, the last spelling wins, and absence leaves
/// `None` (HotSpot's on).
#[test]
fn i85_l2_stack_trace_in_throwable_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:+StackTraceInThrowable",
        "-XX:-StackTraceInThrowable",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:-StackTraceInThrowable"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept -XX:±StackTraceInThrowable");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.stack_trace_in_throwable, Some(false));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.stack_trace_in_throwable.is_none());
}

/// Interpreter round i2 wave 60, lane L1: `-XX:+EnableDynamicAgentLoading`
/// and its `-` form survive the normaliser, the last spelling wins, and
/// absence leaves `None` (the dynamic-agent warning is then due).
#[test]
fn i60_enable_dynamic_agent_loading_is_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:-EnableDynamicAgentLoading",
        "-XX:+EnableDynamicAgentLoading",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:+EnableDynamicAgentLoading"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the flag");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.enable_dynamic_agent_loading, Some(true));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.enable_dynamic_agent_loading.is_none());
}

// -----------------------------------------------------------------------
// expand_aggregate_jars tests
// -----------------------------------------------------------------------

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("cratonvm-cli-{label}-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn expand_missing_netty_all_substitutes_split_jars() {
    let dir = unique_temp_dir("netty-all");
    // Create split jars next to where netty-all.jar would live.
    for name in ["netty-common.jar", "netty-transport.jar", "other.jar"] {
        std::fs::write(dir.join(name), b"pk").unwrap();
    }
    let missing = dir.join("netty-all.jar").to_string_lossy().into_owned();
    let expanded = expand_aggregate_jars(vec![missing.clone()]);
    // netty-all.jar should be gone; the two netty split jars should be
    // present; "other.jar" should NOT have been pulled in.
    assert!(
        !expanded.iter().any(|e| e == &missing),
        "missing aggregate jar must be stripped, got {expanded:?}"
    );
    let has = |n: &str| expanded.iter().any(|e| e.to_ascii_lowercase().ends_with(n));
    assert!(
        has("netty-common.jar"),
        "missing netty-common: {expanded:?}"
    );
    assert!(
        has("netty-transport.jar"),
        "missing netty-transport: {expanded:?}"
    );
    assert!(
        !has("other.jar"),
        "unexpected 'other.jar' in result: {expanded:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn expand_preserves_existing_aggregate() {
    let dir = unique_temp_dir("netty-all-real");
    let aggregate = dir.join("netty-all.jar");
    std::fs::write(&aggregate, b"pk").unwrap();
    // A sibling split jar also exists; we must NOT add it because the
    // aggregate is present and authoritative.
    std::fs::write(dir.join("netty-common.jar"), b"pk").unwrap();
    let entry = aggregate.to_string_lossy().into_owned();
    let expanded = expand_aggregate_jars(vec![entry.clone()]);
    assert_eq!(expanded, vec![entry]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn non_jar_archive_staging_cleanup_removes_temp_copy() {
    let dir = unique_temp_dir("stage-war");
    let archive = dir.join("app.war");
    std::fs::write(&archive, b"fake archive").unwrap();

    let (entry, cleanup) = classpath_entry_for_archive(&archive).unwrap();
    let cleanup = cleanup.expect("non-.jar archive should be staged");
    assert_ne!(entry, archive);
    assert_eq!(entry.extension().and_then(|e| e.to_str()), Some("jar"));
    assert!(
        entry.exists(),
        "staged copy should exist while guard is live"
    );
    assert_eq!(std::fs::read(&entry).unwrap(), b"fake archive");

    drop(cleanup);
    assert!(
        !entry.exists(),
        "staged copy should be removed on guard drop"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn expand_keeps_missing_entry_when_no_siblings() {
    let dir = unique_temp_dir("netty-lonely");
    let missing = dir.join("netty-all.jar").to_string_lossy().into_owned();
    let expanded = expand_aggregate_jars(vec![missing.clone()]);
    // No split jars exist, so the entry is kept as-is (so downstream
    // logging still surfaces the missing-jar message).
    assert_eq!(expanded, vec![missing]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn expand_ignores_non_aggregate_names() {
    let dir = unique_temp_dir("no-match");
    let missing = dir
        .join("does-not-exist.jar")
        .to_string_lossy()
        .into_owned();
    let expanded = expand_aggregate_jars(vec![missing.clone()]);
    assert_eq!(expanded, vec![missing]);
    let _ = std::fs::remove_dir_all(dir);
}

// -----------------------------------------------------------------------
// `--sun-misc-unsafe-memory-access=<mode>` (JEP 498).
//
// The flag's whole effect is the system property it sets, and the property
// is the single input netty 4.2 keys its Unsafe-vs-FFM decision on. A
// default run must leave it UNSET, exactly as HotSpot does — CratonVM used
// to pin it to `allow` in `vm_init`, which silently put netty and every
// other Unsafe-aware library on a different code path than a stock JDK 25.
// -----------------------------------------------------------------------

#[test]
fn unsafe_memory_access_flag_becomes_the_system_property() {
    let out = normalize_java_launcher_argv(argv(&[
        "java",
        "--sun-misc-unsafe-memory-access=allow",
        "Main",
    ]));
    assert_eq!(
        out,
        argv(&["java", "-Dsun.misc.unsafe.memory.access=allow", "Main"])
    );
}

#[test]
fn every_unsafe_memory_access_mode_round_trips() {
    for mode in ["allow", "warn", "debug", "deny"] {
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            &format!("--sun-misc-unsafe-memory-access={mode}"),
            "Main",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                &format!("-Dsun.misc.unsafe.memory.access={mode}"),
                "Main"
            ]),
            "mode {mode}"
        );
    }
}

#[test]
fn no_unsafe_memory_access_flag_leaves_the_property_unset() {
    // The load-bearing half: a plain command line must not introduce the
    // property, because `System.getProperty(...) == null` is what makes
    // netty take the same path it takes on a stock JDK 25.
    let out = normalize_java_launcher_argv(argv(&["java", "-Xmx1g", "Main"]));
    assert!(
        !out.iter()
            .any(|a| a.contains("sun.misc.unsafe.memory.access")),
        "a default command line must not mention the property: {out:?}"
    );
}

#[test]
fn an_unsafe_memory_access_flag_after_the_separator_is_the_programs_own() {
    let out = normalize_java_launcher_argv(argv(&[
        "java",
        "Main",
        "--",
        "--sun-misc-unsafe-memory-access=allow",
    ]));
    assert_eq!(
        out,
        argv(&[
            "java",
            "Main",
            "--",
            "--sun-misc-unsafe-memory-access=allow"
        ])
    );
}

// -----------------------------------------------------------------------
// Assertion flags: `-ea` and friends.
//
// Two halves that must both hold. `normalize_java_launcher_argv` still
// *strips* every spelling (clap cannot parse them), and
// `launcher_assertions_requested` reads the same argv for the switch. A
// test that only checked the strip would pass with the switch deleted,
// which is exactly the state this fixed.
// -----------------------------------------------------------------------

#[test]
fn assertion_flags_are_stripped_before_clap() {
    for flag in [
        "-ea",
        "-da",
        "-esa",
        "-dsa",
        "-enableassertions",
        "-disableassertions",
        "-enablesystemassertions",
        "-disablesystemassertions",
        "-ea:io.netty...",
        "-da:some.Class",
    ] {
        let out = normalize_java_launcher_argv(argv(&["java", flag, "Main"]));
        assert_eq!(
            out,
            argv(&["java", "Main"]),
            "flag {flag} survived the strip"
        );
    }
}

#[test]
fn unscoped_ea_enables_and_da_disables() {
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-ea", "Main"])),
        Some(true)
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-enableassertions", "Main"])),
        Some(true)
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-da", "Main"])),
        Some(false)
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-disableassertions", "Main"])),
        Some(false)
    );
}

#[test]
fn no_assertion_flag_leaves_the_env_var_in_charge() {
    // `None`, not `Some(false)`: a plain command line must not clear an
    // inherited CRATONVM_ENABLE_ASSERTIONS.
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-Xmx1g", "Main"])),
        None
    );
}

#[test]
fn scoped_assertion_flags_do_not_touch_the_user_default() {
    // `-ea:io.netty` must NOT read as global-enable — that would turn
    // assertions on for every class the caller deliberately left out. It
    // travels in the directives instead (wave 107, lane L5).
    for flag in ["-ea:io.netty...", "-da:io.netty.Foo", "-ea:com.example"] {
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", flag, "Main"])),
            None,
            "scoped flag {flag} was honoured"
        );
    }
    let d = launcher_assertion_directives(&argv(&[
        "java",
        "-ea:io.netty...",
        "-da:io.netty.Foo",
        "-esa",
        "Main",
        "--",
        "-da",
    ]))
    .expect("directives");
    assert_eq!(d.user_default, None);
    assert!(d.system_default);
    assert_eq!(d.packages, vec![("io/netty".to_string(), true)]);
    assert_eq!(d.classes, vec![("io/netty/Foo".to_string(), false)]);
    assert!(d.enabled("io/netty/Bar", false, false));
    assert!(!d.enabled("io/netty/Foo", false, true));
    assert_eq!(
        launcher_assertion_directives(&argv(&["java", "-Xmx1g", "Main", "--", "-ea"])),
        None
    );
}

#[test]
fn a_long_assertion_spelling_with_a_bad_tail_is_refused() {
    // Wave 107 (lane L5): HotSpot answers "Unrecognized option".
    for bad in [
        "-enableassertionsX",
        "-enablesystemassertions:p...",
        "-disablesystemassertionsX",
    ] {
        assert_eq!(
            refused_assertion_spelling(&argv(&["java", bad, "Main"])),
            Some(bad)
        );
    }
    for good in ["-enableassertions:p...", "-disableassertions", "-ea:p", "-esa"] {
        assert_eq!(refused_assertion_spelling(&argv(&["java", good, "Main"])), None);
    }
    assert_eq!(
        refused_assertion_spelling(&argv(&["java", "Main", "--", "-enableassertionsX"])),
        None
    );
}

#[test]
fn last_wins_within_a_scope() {
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-ea", "-da", "Main"])),
        Some(false)
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-da", "-ea", "Main"])),
        Some(true)
    );
}

#[test]
fn a_system_scope_flag_never_cancels_a_user_scope_enable() {
    // HotSpot's `-ea -dsa` is "user assertions on, system assertions off":
    // the `-dsa` must not undo the `-ea` Surefire put there. Since wave 107
    // (lane L5) the system switches are their own scope (the directives),
    // so they leave the user default alone entirely.
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-ea", "-dsa", "Main"])),
        Some(true)
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-esa", "-da", "Main"])),
        Some(false)
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-dsa", "Main"])),
        None
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-esa", "Main"])),
        None
    );
    let d = launcher_assertion_directives(&argv(&["java", "-da", "-esa", "Main"]))
        .expect("directives");
    assert_eq!(d.user_default, Some(false));
    assert!(d.system_default);
    assert!(!d.enabled("Main", false, true));
    assert!(d.enabled("java/lang/String", true, false));
}

#[test]
fn an_ea_after_the_program_args_separator_is_the_programs_own() {
    // `java -jar app.jar -- -ea` passes `-ea` to the application; scanning
    // past `--` would let a program argument reconfigure the VM.
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "Main", "--", "-ea"])),
        None
    );
    assert_eq!(
        launcher_assertions_requested(&argv(&["java", "-da", "Main", "--", "-ea"])),
        Some(false)
    );
}

// -----------------------------------------------------------------------
// HotSpot single-dash compatibility rewrites — C36 acceptance.
//
// Stock `java -Xmx256m -classpath x Main` must parse, otherwise the
// `[[bin]] name = "java"` alias is useless for Maven Surefire / Gradle.
// -----------------------------------------------------------------------

#[test]
fn hotspot_xmx_inline_rewrites_to_clap_long() {
    let raw = argv(&["java", "-Xmx256m", "-classpath", "x", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&["java", "--Xmx", "256m", "--classpath", "x", "Main"])
    );
}

#[test]
fn hotspot_xmx_separate_token_rewrites_to_clap_long() {
    let raw = argv(&["java", "-Xmx", "1g", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--Xmx", "1g", "Main"]));
}

#[test]
fn hotspot_xms_inline_rewrites_to_clap_long() {
    // F-16: `-Xms512m` used to be dropped, because the heap was allocated
    // at `-Xmx` in the collector's constructor and an initial size named
    // nothing. G1 now reserves `-Xmx` and commits a prefix, so the value is
    // carried through to clap like `-Xmx`'s.
    let raw = argv(&["java", "-Xms512m", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--Xms", "512m", "Main"]));
}

#[test]
fn a_bare_trailing_xms_with_no_value_is_still_dropped() {
    // Nothing to carry, and emitting `--Xms` with no value would make clap
    // reject a command line HotSpot accepts.
    let raw = argv(&["java", "Main", "-Xms"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "Main"]));
}

#[test]
fn hotspot_xms_separate_token_keeps_its_value_and_the_class_name() {
    // B1 regression, still: `-Xms 512m` (separate value token) must not
    // leave a bare `512m` to be mistaken for the main-class positional,
    // shifting `Main` into a program arg. Maven Surefire / Gradle forks
    // emit this form. The value sits adjacent here because `-Xms` is in
    // VALUE_TAKING_OPTS, so this mirrors the full pre-clap pipeline.
    //
    // F-16 changed the remedy, not the requirement: the value is now
    // consumed by being handed to clap as `--Xms 512m` rather than by being
    // thrown away.
    let stage1 = insert_program_args_separator(argv(&["java", "-Xms", "512m", "Main"]));
    let out = normalize_java_launcher_argv(stage1);
    assert_eq!(out, argv(&["java", "--Xms", "512m", "Main", "--"]));
}

#[test]
fn hotspot_xms_separate_token_passes_clap_after_full_pipeline() {
    // B1 acceptance: stock `java -Xms512m -Xmx256m -classpath x Main`
    // with the separate-token `-Xms 512m` form must parse end-to-end with
    // `Main` resolved as the main class (not the heap-size value `512m`).
    let argv0: Vec<String> = argv(&[
        "java",
        "-Xms",
        "512m",
        "-Xmx",
        "256m",
        "-classpath",
        "x",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept HotSpot separate-token -Xms");
    assert_eq!(parsed.max_heap.as_deref(), Some("256m"));
    // F-16: and the initial size ARRIVES, rather than being discarded on
    // the way. (`-Xms` above `-Xmx` is clamped where the config is built,
    // not here — this stage only has to carry the value.)
    assert_eq!(parsed.initial_heap.as_deref(), Some("512m"));
    assert_eq!(parsed.classpath.as_deref(), Some("x"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

/// gen r4w2/alloc2: `-Xmn` used to fall into the `-X…` catch-all and be
/// dropped. Both spellings now reach clap as `--Xmn <size>`, and a bare
/// trailing `-Xmn` is dropped like a bare `-Xms`.
#[test]
fn hotspot_xmn_inline_and_trailing_forms() {
    let out = normalize_java_launcher_argv(argv(&["java", "-Xmn256m", "Main"]));
    assert_eq!(out, argv(&["java", "--Xmn", "256m", "Main"]));
    let bare = normalize_java_launcher_argv(argv(&["java", "Main", "-Xmn"]));
    assert_eq!(bare, argv(&["java", "Main"]));
}

/// The separate-token form must not leave `256m` behind as the main class
/// — the reason `-Xmn` is in `VALUE_TAKING_OPTS` — and the four young
/// sizing flags must all arrive at clap.
#[test]
fn hotspot_young_sizing_flags_pass_clap_after_full_pipeline() {
    let argv0: Vec<String> = argv(&[
        "java",
        "-Xmn",
        "256m",
        "-XX:NewSize=64m",
        "-XX:MaxNewSize=512m",
        "-XX:NewRatio=3",
        "-Xmx",
        "1g",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the young flags");
    assert_eq!(parsed.young_xmn.as_deref(), Some("256m"));
    assert_eq!(parsed.young_new_size.as_deref(), Some("64m"));
    assert_eq!(parsed.young_max_new_size.as_deref(), Some("512m"));
    assert_eq!(parsed.young_new_ratio.as_deref(), Some("3"));
    assert_eq!(parsed.max_heap.as_deref(), Some("1g"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert!(VALUE_TAKING_OPTS.contains(&"-Xmn"));
    assert!(VALUE_TAKING_OPTS.contains(&"--Xmn"));
}

#[test]
fn hotspot_xshare_colon_rewrites_to_clap_long() {
    let raw = argv(&["java", "-Xshare:on", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--Xshare", "on", "Main"]));
}

#[test]
fn hotspot_xverify_none_collapses_to_noverify_flag() {
    let raw = argv(&["java", "-Xverify:none", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--noverify", "Main"]));
}

/// Interpreter round i5 wave 101, lane L7.
#[test]
fn hotspot_bytecode_verification_remote_off_is_noverify() {
    let raw = argv(&["java", "-XX:-BytecodeVerificationRemote", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--noverify", "Main"]));
    let raw = argv(&["java", "-XX:+BytecodeVerificationRemote", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "Main"]));
}

#[test]
fn hotspot_xverify_remote_rewrites_to_clap_long() {
    let raw = argv(&["java", "-Xverify:remote", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--Xverify", "remote", "Main"]));
}

#[test]
fn hotspot_xbootclasspath_inline_rewrites_to_clap_long() {
    let raw = argv(&["java", "-Xbootclasspath:/opt/boot", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&["java", "--Xbootclasspath", "/opt/boot", "Main"])
    );
}

#[test]
fn hotspot_xbootclasspath_append_and_prepend_normalise() {
    // Round 13 wave 10 (lane misc10): /a (append) no longer collapses onto
    // the replace, which dropped the JDK image; /p (prepend) gets its own
    // spelling so `run` can refuse it as HotSpot 9+ does.
    let raw = argv(&["java", "-Xbootclasspath/a:/opt/extra", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&["java", "--Xbootclasspath-append", "/opt/extra", "Main"])
    );

    let raw = argv(&["java", "-Xbootclasspath/p:/opt/pre", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&["java", "--Xbootclasspath-prepend", "/opt/pre", "Main"])
    );
}

/// `-Xbootclasspath/a:` is repeatable (HotSpot accumulates it in order) and
/// leaves the replace (`boot_classpath`) unset, so the image is still
/// discovered.
#[test]
fn hotspot_xbootclasspath_append_is_repeatable_and_keeps_the_image() {
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(argv(&[
        "java",
        "-Xbootclasspath/a:/opt/one",
        "-Xbootclasspath/a:/opt/two",
        "Main",
    ])))
    .expect("two -Xbootclasspath/a: must parse after normalisation");
    assert_eq!(parsed.boot_classpath_append, vec!["/opt/one", "/opt/two"]);
    assert!(parsed.boot_classpath.is_none());
    assert!(parsed.boot_classpath_prepend.is_none());
}


#[test]
fn hotspot_xlog_colon_rewrites_to_clap_long() {
    let raw = argv(&["java", "-Xlog:gc*=info", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--Xlog", "gc*=info", "Main"]));
}

/// gen r5w3/obs7: HotSpot takes `-Xlog` any number of times
/// (`-Xlog:gc -Xlog:safepoint:file=sp.log`); clap refused the second
/// one, so such a command line never started. All are kept, in order.
#[test]
fn hotspot_xlog_is_repeatable() {
    let parsed = Args::try_parse_from(normalize_java_launcher_argv(argv(&[
        "java",
        "-Xlog:gc",
        "-Xlog:gc+heap=debug:file=gc.log",
        "Main",
    ])))
    .expect("two -Xlog options must parse");
    assert_eq!(parsed.xlog, ["gc", "gc+heap=debug:file=gc.log"]);
    assert_eq!(parsed.xlog.join(";"), "gc;gc+heap=debug:file=gc.log");
}

/// gen r5w4/obs8 (`gengc-r5w3-obs7-xlog-bare-option-eats-the-next-argument`):
/// a bare `-Xlog` is HotSpot's `-Xlog:all` and takes no operand. It used
/// to consume the next token as its spec, so `-cp` became the log
/// selection and the class path the main class. Full launcher pipeline:
/// the separator inserter must not treat it as value-taking either, or
/// `java -Xlog Main arg` loses `Main` to it.
#[test]
fn hotspot_bare_xlog_is_all_and_takes_no_operand() {
    fn launch(parts: &[&str]) -> Args {
        let stage1 = insert_program_args_separator(argv(parts));
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        Args::try_parse_from(stage4).expect("clap must parse the command line")
    }

    let parsed = launch(&["java", "-Xlog", "-cp", "/tmp/cp", "Main", "a1"]);
    assert_eq!(parsed.xlog, ["all"]);
    assert_eq!(parsed.classpath.as_deref(), Some("/tmp/cp"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parsed.args, argv(&["a1"]));

    let parsed = launch(&["java", "-Xlog", "Main", "a1"]);
    assert_eq!(parsed.xlog, ["all"]);
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parsed.args, argv(&["a1"]));

    // Mixed with the colon form, in order.
    let parsed = launch(&["java", "-Xlog:gc", "-Xlog", "Main"]);
    assert_eq!(parsed.xlog, ["gc", "all"]);
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));

    // `-Xlog` after the main class is the program's own argument.
    let parsed = launch(&["java", "Main", "-Xlog"]);
    assert!(parsed.xlog.is_empty());
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parsed.args, argv(&["-Xlog"]));
}

#[test]
fn hotspot_xx_shared_archive_file_rewrites_to_clap_long() {
    let raw = argv(&["java", "-XX:SharedArchiveFile=app.jsa", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&["java", "--XX:SharedArchiveFile", "app.jsa", "Main"])
    );
}

/// F29-1 §5 NOMINATION 5: `-XX:AutoBoxCacheMax=N` is MEASURED
/// byte-identical to `-Djava.lang.Integer.IntegerCache.high=N` on HotSpot
/// 25, so it is rewritten onto that property before `-D` extraction runs,
/// rather than given a second, parallel cache-configuration road.
#[test]
fn hotspot_xx_autoboxcachemax_rewrites_to_the_integer_cache_property() {
    let raw = argv(&["java", "-XX:AutoBoxCacheMax=1000", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&["java", "-Djava.lang.Integer.IntegerCache.high=1000", "Main"])
    );
    // End to end: the rewritten token is a genuine `-D` property by the
    // time `extract_system_properties` sees it, not just clap-parseable.
    let (filtered, props) = extract_system_properties(out);
    assert_eq!(filtered, argv(&["java", "Main"]));
    assert_eq!(
        props,
        vec![(
            "java.lang.Integer.IntegerCache.high".to_string(),
            "1000".to_string()
        )]
    );
}

#[test]
fn hotspot_xx_aot_flags_rewrite_to_clap_long() {
    let raw = argv(&[
        "java",
        "-XX:AOTMode=training",
        "-XX:AOTCache=in.aot",
        "-XX:AOTCacheOutput=out.aot",
        "Main",
    ]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(
        out,
        argv(&[
            "java",
            "--XX:AOTMode",
            "training",
            "--XX:AOTCache",
            "in.aot",
            "--XX:AOTCacheOutput",
            "out.aot",
            "Main",
        ])
    );
}

#[test]
fn hotspot_xx_use_container_support_toggle() {
    // `-XX:-UseContainerSupport` -> clap long form.
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:-UseContainerSupport", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:-UseContainerSupport", "Main"]));
    // `-XX:+UseContainerSupport` is the default; gets dropped.
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseContainerSupport", "Main"]));
    assert_eq!(out, argv(&["java", "Main"]));
}

#[test]
fn hotspot_xx_useg1gc_normalizes_to_selector() {
    // `-XX:+UseG1GC` -> `--XX:UseGc G1` (value option, adjacent value).
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseG1GC", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:UseGc", "G1", "Main"]));
    // `-XX:-UseG1GC` explicitly reverts to the default Generational.
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:-UseG1GC", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:UseGc", "Generational", "Main"]));
}

/// zt t3/ob (`zgc-zs3rv2-wave-s2-review-residuals` §8): turning off a
/// collector the command line did not select keeps the one it selected,
/// as HotSpot 25 does; turning off the selected one (or with none
/// selected) still selects Generational. Positive control: the kill arm
/// deselects whatever was selected, as before.
#[test]
fn zt3ob_turning_off_another_collector_keeps_the_selected_one() {
    let out =
        normalize_java_launcher_argv(argv(&["java", "-XX:+UseZGC", "-XX:-UseG1GC", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:UseGc", "Z", "Main"]));
    let out =
        normalize_java_launcher_argv(argv(&["java", "-XX:+UseG1GC", "-XX:-UseZGC", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:UseGc", "G1", "Main"]));
    let out =
        normalize_java_launcher_argv(argv(&["java", "-XX:+UseG1GC", "-XX:-UseG1GC", "Main"]));
    assert_eq!(
        out,
        argv(&["java", "--XX:UseGc", "G1", "--XX:UseGc", "Generational", "Main"])
    );
    assert!(minus_gc_flag_deselects_with("-XX:-UseG1GC", None, true));
    assert!(!minus_gc_flag_deselects_with("-XX:-UseG1GC", Some("Z"), true));
    assert!(minus_gc_flag_deselects_with("-XX:-UseZGC", Some("Z"), true));
    assert!(!minus_gc_flag_deselects_with("-XX:-UseZGC", Some("Generational"), true));
    assert!(minus_gc_flag_deselects_with("-XX:-UseG1GC", Some("Z"), false), "kill");
    let raw = argv(&["java", "-XX:+UseZGC", "-XX:-UseG1GC", "-version"]);
    if cfg!(feature = "zgc") {
        assert!(scan_effective_zgc(&raw));
    }
}

#[test]
fn hotspot_xx_unsupported_gc_is_forwarded_not_dropped() {
    // Selectors are forwarded verbatim; support validation happens at
    // config-apply time (`parse_gc_algorithm`), not here.
    for (flag, name) in [
        ("-XX:+UseParallelGC", "Parallel"),
        ("-XX:+UseSerialGC", "Serial"),
        ("-XX:+UseShenandoahGC", "Shenandoah"),
    ] {
        let out = normalize_java_launcher_argv(argv(&["java", flag, "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", name, "Main"]), "{flag}");
    }
}

#[test]
fn hotspot_xx_usezgc_normalizes_to_selector() {
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseZGC", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:UseGc", "Z", "Main"]));
}

#[test]
fn hotspot_usezgc_reaches_clap_as_selector_after_pipeline() {
    let argv0: Vec<String> = argv(&["java", "-XX:+UseZGC", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:+UseZGC");
    assert_eq!(parsed.gc_selector.as_deref(), Some("Z"));
}

#[test]
fn hotspot_xx_non_gc_use_flags_not_mistaken_for_selector() {
    // `-XX:+Use*` flags that do NOT end in `GC` must not become a GC
    // selector. Supported non-GC flags keep their own mapping.
    let out =
        normalize_java_launcher_argv(argv(&["java", "-XX:+UseStringDeduplication", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:StringDedup", "true", "Main"]));

    let out =
        normalize_java_launcher_argv(argv(&["java", "-XX:-UseStringDeduplication", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:StringDedup", "false", "Main"]));

    let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseCompressedOops", "Main"]));
    assert_eq!(out, argv(&["java", "Main"]));
}

#[test]
fn hotspot_useg1gc_reaches_clap_as_selector_after_pipeline() {
    // End-to-end: `-XX:+UseG1GC` survives the full pre-clap pipeline and
    // lands in `Args::gc_selector`, the field the config-apply step reads.
    let argv0: Vec<String> = argv(&["java", "-XX:+UseG1GC", "-classpath", "x", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:+UseG1GC");
    assert_eq!(parsed.gc_selector.as_deref(), Some("G1"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

#[test]
fn hotspot_string_dedup_reaches_clap_after_pipeline() {
    let argv0: Vec<String> = argv(&["java", "-XX:+UseStringDeduplication", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept string dedup flag");
    assert_eq!(parsed.g1_string_dedup.as_deref(), Some("true"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

#[test]
fn hotspot_repeated_gc_flags_last_wins() {
    // HotSpot honours the last `-XX:+Use*GC`; clap's `Set` action keeps the
    // last value, so a `-XX:+UseParallelGC -XX:+UseG1GC` pair selects G1.
    let argv0: Vec<String> = argv(&["java", "-XX:+UseParallelGC", "-XX:+UseG1GC", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept repeated GC flags");
    assert_eq!(parsed.gc_selector.as_deref(), Some("G1"));
}

#[test]
fn hotspot_xx_audit_missing_natives_toggle() {
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:+AuditMissingNatives", "Main"]));
    assert_eq!(out, argv(&["java", "--XX:AuditMissingNatives", "Main"]));
    // Disabled form drops the flag (default is off).
    let out = normalize_java_launcher_argv(argv(&["java", "-XX:-AuditMissingNatives", "Main"]));
    assert_eq!(out, argv(&["java", "Main"]));
}

#[test]
fn hotspot_xx_show_code_details_toggle() {
    // `-XX:+ShowCodeDetailsInExceptionMessages` -> the explicit `=true` form.
    let out = normalize_java_launcher_argv(argv(&[
        "java",
        "-XX:+ShowCodeDetailsInExceptionMessages",
        "Main",
    ]));
    assert_eq!(
        out,
        argv(&[
            "java",
            "--XX:ShowCodeDetailsInExceptionMessages=true",
            "Main"
        ])
    );
    // Disabled form -> the explicit `=false` form (the default is now on,
    // so opting out must be representable, not merely dropped).
    let out = normalize_java_launcher_argv(argv(&[
        "java",
        "-XX:-ShowCodeDetailsInExceptionMessages",
        "Main",
    ]));
    assert_eq!(
        out,
        argv(&[
            "java",
            "--XX:ShowCodeDetailsInExceptionMessages=false",
            "Main"
        ])
    );
}

#[test]
fn hotspot_noverify_rewrites_to_clap_long() {
    let raw = argv(&["java", "-noverify", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--noverify", "Main"]));
}

#[test]
fn hotspot_xmx_after_separator_is_program_arg() {
    // Tokens past `--` belong to the Java program, not the launcher.
    let raw = argv(&["java", "Main", "--", "-Xmx256m"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "Main", "--", "-Xmx256m"]));
}

#[test]
fn hotspot_xmx_passes_clap_after_full_pipeline() {
    // C36 acceptance test: stock `java -Xmx256m -classpath x Main`
    // must parse end-to-end. Exercises the entire pre-clap pipeline
    // exactly as `run()` would.
    let argv0: Vec<String> = argv(&["java", "-Xmx256m", "-classpath", "x", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept HotSpot -Xmx");
    assert_eq!(parsed.max_heap.as_deref(), Some("256m"));
    assert_eq!(parsed.classpath.as_deref(), Some("x"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

#[test]
fn nojit_flag_is_accepted_by_clap() {
    // README documents `--nojit`; clap must accept it.
    let parsed = Args::try_parse_from(argv(&["cratonvm", "--nojit", "Main"]))
        .expect("clap must accept --nojit");
    assert!(parsed.nojit);
}

/// The `CRATONVM_LAUNCHER_NATIVE_ACCESS_SPACE_FORM=0` arm (this pipeline
/// without [`join_enable_native_access_space_form`]): a bare flag grants
/// `ALL-UNNAMED` and the next token is the main class. HotSpot's reading
/// is the default since zt t16/tg7:
/// `zt16tg7_enable_native_access_takes_its_separate_value_as_libjli`.
#[test]
fn enable_native_access_bare_flag_does_not_consume_main_class() {
    let argv0: Vec<String> = argv(&["java", "--enable-native-access", "Main", "arg"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse bare native-access flag");

    assert_eq!(parsed.enable_native_access.as_deref(), Some("ALL-UNNAMED"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parsed.args, argv(&["arg"]));
}

#[test]
fn enable_preview_is_a_bare_flag_and_does_not_consume_main_class() {
    // Before docs/feature-designs/class-resolution-and-caching.md
    // this was `error: unexpected argument '--enable-preview' found`, with
    // clap suggesting `--enable-native-access`. Run the whole pre-clap
    // pipeline, not just `try_parse_from`: a bare boolean needs no
    // VALUE_TAKING_OPTS entry, and this is what proves it — if it ever
    // acquired one, `Main` would be eaten as the flag's value.
    let argv0: Vec<String> = argv(&["java", "--enable-preview", "Main", "arg"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse --enable-preview");

    assert!(parsed.enable_preview);
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parsed.args, argv(&["arg"]));
}

#[test]
fn enable_preview_defaults_off_like_hotspot() {
    // HotSpot's default is off, measured: plain `java -cp . P` on a
    // 69.65535 class file raises UnsupportedClassVersionError.
    let parsed = Args::try_parse_from(argv(&["cratonvm", "Main"]))
        .expect("clap must parse without the flag");
    assert!(!parsed.enable_preview);
}

/// zt t18/hunt2: `--finalization=<value>` (JEP 421) as HotSpot 25's
/// `parse_each_vm_init_arg` reads it: `enabled` / `disabled`, the last one
/// wins, any other value refused (the value is what the message quotes),
/// absent = the default, and a program argument after `--` is never read.
#[test]
fn t18_hunt2_finalization_option_is_read_like_hotspot() {
    assert_eq!(launcher_finalization_option(&argv(&["java", "Main"])), Ok(None));
    assert_eq!(
        launcher_finalization_option(&argv(&["java", "--finalization=disabled", "Main"])),
        Ok(Some(false))
    );
    assert_eq!(
        launcher_finalization_option(&argv(&[
            "java",
            "--finalization=disabled",
            "--finalization=enabled",
            "Main",
        ])),
        Ok(Some(true))
    );
    assert_eq!(
        launcher_finalization_option(&argv(&["java", "--finalization=off", "Main"])),
        Err("off".to_string())
    );
    assert_eq!(
        launcher_finalization_option(&argv(&["java", "Main", "--", "--finalization=bogus"])),
        Ok(None)
    );
}

/// zt t18/hunt2: the option no longer reaches clap (which refused it as an
/// unexpected argument) and does not take the main class as a value; it is
/// recorded among the VM options (`RuntimeMXBean.getInputArguments()`).
#[test]
fn t18_hunt2_finalization_option_is_stripped_before_clap_and_recorded() {
    let argv0: Vec<String> = argv(&["java", "--finalization=disabled", "Main", "arg"]);
    let stage1 = insert_program_args_separator(argv0);
    assert_eq!(hotspot_jvm_args(&stage1), ["--finalization=disabled"]);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(!stage2.iter().any(|a| a.starts_with("--finalization")));
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse without the option");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(parsed.args, argv(&["arg"]));
}

#[test]
fn enable_native_access_equals_value_is_still_accepted() {
    let parsed = Args::try_parse_from(argv(&[
        "cratonvm",
        "--enable-native-access=java.base",
        "Main",
    ]))
    .expect("clap must parse native-access value with equals");

    assert_eq!(parsed.enable_native_access.as_deref(), Some("java.base"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

#[test]
fn hotspot_xss_inline_is_accepted() {
    // B3 regression: single-dash `-Xss<size>` (thread stack size —
    // Surefire/Gradle pass this routinely) was not in the handled set, so
    // it fell through to the final `else`, reached clap verbatim, and clap
    // rejected it as "unexpected argument '-X'". The catch-all `-X` arm
    // then accepted-and-ignored it.
    //
    // gcd d6/f: it is honoured now (`VmConfig::default_thread_stack_size`),
    // so the normaliser rewrites it to clap's `--Xss <size>` instead of
    // dropping it; a bare `-Xss` is still dropped.
    let raw = argv(&["java", "-Xss512k", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "--Xss", "512k", "Main"]));
    let bare = normalize_java_launcher_argv(argv(&["java", "-Xss", "Main"]));
    assert_eq!(bare, argv(&["java", "Main"]));
}

#[test]
fn hotspot_misc_x_flags_are_accepted_and_ignored() {
    // Other no-value `-X` knobs HotSpot accepts: `-Xint`, `-Xbatch`,
    // `-XshowSettings`. All drop out, leaving the class name (and any
    // later args) intact. (`-Xrs` is kept since wave 79:
    // `i79_reduce_signal_usage_is_parsed`; `-Xnoclassgc` since zt t21/tg10:
    // `zt21tg10_class_unloading_options_are_parsed`.)
    let raw = argv(&["java", "-Xint", "-Xbatch", "-XshowSettings", "Main"]);
    let out = normalize_java_launcher_argv(raw);
    assert_eq!(out, argv(&["java", "Main"]));
}

/// zt t21/tg10 (`CRATONVM_LAUNCHER_USAGE_XLOG_BOOT`): a usage or help
/// text boots a VM first only when the launcher section selects `-Xlog`
/// output (the version boot's rule), and never under the `=0` kill.


/// zt t21/tg10: `-Xnoclassgc` and `-XX:±ClassUnloading` survive the
/// normaliser, clap never sees them, the last spelling wins, and absence
/// leaves `None` (HotSpot's default, class unloading on).
#[test]
fn zt21tg10_class_unloading_options_are_parsed() {
    let argv0 = argv(&["java", "-XX:+ClassUnloading", "-Xnoclassgc", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-Xnoclassgc"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    assert!(
        !stage4.iter().any(|a| a.contains("ClassUnloading") || a == "-Xnoclassgc"),
        "stripped before clap: {stage4:?}"
    );
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the rest");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.class_unloading, Some(false));
    let (_, on) = extract_hotspot_flags(argv(&[
        "cratonvm",
        "-XX:-ClassUnloading",
        "-XX:+ClassUnloading",
        "Main",
    ]));
    assert_eq!(on.class_unloading, Some(true));
    let (_, none) = extract_hotspot_flags(argv(&["cratonvm", "Main"]));
    assert!(none.class_unloading.is_none());
}

/// zt t23/hunt4: `-XX:±AlwaysPreTouch` survives the normaliser (the
/// `-XX:` catch-all dropped it), clap never sees it, the last spelling
/// wins, and absence leaves `None` (HotSpot's default, off).
#[test]
fn zt23hunt4_always_pre_touch_is_parsed() {
    let argv0 = argv(&["java", "-XX:-AlwaysPreTouch", "-XX:+AlwaysPreTouch", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:+AlwaysPreTouch"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    assert!(
        !stage4.iter().any(|a| a.contains("AlwaysPreTouch")),
        "stripped before clap: {stage4:?}"
    );
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the rest");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.always_pre_touch, Some(true));
    let (_, off) = extract_hotspot_flags(argv(&[
        "cratonvm",
        "-XX:+AlwaysPreTouch",
        "-XX:-AlwaysPreTouch",
        "Main",
    ]));
    assert_eq!(off.always_pre_touch, Some(false));
    let (_, none) = extract_hotspot_flags(argv(&["cratonvm", "Main"]));
    assert!(none.always_pre_touch.is_none());
}

/// gcj j1/st: `-XX:±PrintStringTableStatistics` and `-XX:StringTableSize=<n>`
/// survive the normaliser (the `-XX:` catch-all dropped them), clap never
/// sees them, the last spelling wins, and absence leaves `None`.
#[test]
fn gcj_j1st_string_table_flags_are_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:-PrintStringTableStatistics",
        "-XX:+PrintStringTableStatistics",
        "-XX:StringTableSize=1000",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:+PrintStringTableStatistics")
            && stage2.iter().any(|a| a == "-XX:StringTableSize=1000"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    assert!(
        !stage4.iter().any(|a| a.contains("StringTable")),
        "stripped before clap: {stage4:?}"
    );
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the rest");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.print_string_table_statistics, Some(true));
    assert_eq!(flags.string_table_size, Some(1000));
    assert!(print_string_table_statistics_requested(&argv(&[
        "java",
        "-XX:+PrintStringTableStatistics",
        "-version",
    ])));
    assert!(!print_string_table_statistics_requested(&argv(&[
        "java",
        "-XX:+PrintStringTableStatistics",
        "-XX:-PrintStringTableStatistics",
        "-version",
    ])));
    let (_, none) = extract_hotspot_flags(argv(&["cratonvm", "Main"]));
    assert!(none.print_string_table_statistics.is_none());
    assert!(none.string_table_size.is_none());
}

/// zt t24/hunt7: `-XX:±UseTransparentHugePages` and `-XX:±UseLargePages`
/// survive the normaliser (the `-XX:` catch-all dropped them), clap never
/// sees them, the last spelling wins, and an explicit `-XX:-UseLargePages`
/// turns transparent huge pages off, as HotSpot's `os::large_page_init`
/// does. Off Linux the flag is never in force.
#[test]
fn zt24hunt7_transparent_huge_pages_flags_are_parsed() {
    let argv0 = argv(&[
        "java",
        "-XX:-UseTransparentHugePages",
        "-XX:+UseTransparentHugePages",
        "-XX:+UseLargePages",
        "Main",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-XX:+UseTransparentHugePages"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    assert!(
        !stage4
            .iter()
            .any(|a| a.contains("HugePages") || a.contains("LargePages")),
        "stripped before clap: {stage4:?}"
    );
    let parsed = Args::try_parse_from(stage4).expect("clap must accept the rest");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.use_transparent_huge_pages, Some(true));
    assert_eq!(flags.use_large_pages, Some(true));
    let (_, none) = extract_hotspot_flags(argv(&["cratonvm", "Main"]));
    assert!(none.use_transparent_huge_pages.is_none());
    assert!(none.use_large_pages.is_none());
    let thp = effective_use_transparent_huge_pages;
    assert!(thp(true, Some(true), Some(true)));
    assert!(thp(true, Some(true), None));
    assert!(!thp(true, Some(true), Some(false)), "-XX:-UseLargePages");
    assert!(!thp(true, Some(false), None));
    assert!(!thp(true, None, Some(true)));
    assert!(!thp(false, Some(true), None), "off Linux");
}

/// zt t23/hunt4: HotSpot 25's obsolete-flag warning for every spelling of
/// `ZGenerational` (`process_argument` strips the sign and the `=` part),
/// nothing for other flags or a near name, program arguments ignored,
/// command-line order kept, and nothing at or after the first refused
/// option.
#[test]
fn zt23hunt4_obsolete_flag_warnings_are_hotspots() {
    const LINE: &str = concat!(
        "OpenJDK 64-Bit Server VM warning: ",
        "Ignoring option ZGenerational; support was removed in 24.0"
    );
    for spelling in [
        "-XX:+ZGenerational",
        "-XX:-ZGenerational",
        "-XX:ZGenerational=true",
        "-XX:ZGenerational",
    ] {
        assert_eq!(
            obsolete_flag_warning_of(spelling).as_deref(),
            Some(LINE),
            "{spelling}"
        );
    }
    for other in [
        "-XX:+UseZGC",
        "-XX:+ZGenerationalX",
        "-ZGenerational",
        "ZGenerational",
    ] {
        assert!(obsolete_flag_warning_of(other).is_none(), "{other}");
    }
    let line = argv(&[
        "java",
        "-XX:+ZGenerational",
        "-Xmx64m",
        "-XX:-ZGenerational",
        "--",
        "-XX:+ZGenerational",
    ]);
    assert_eq!(obsolete_flag_warning_lines(&line, |_| false), [LINE, LINE]);
    // A refusal at `-Xmx64m` (index 2): the warning before it prints, the
    // one after it does not.
    let lines = obsolete_flag_warning_lines(&line, |prefix| prefix.len() > 2);
    assert_eq!(lines, [LINE]);
    assert!(obsolete_flag_warning_lines(&argv(&["java", "Main"]), |_| false).is_empty());
}

/// t1 wave 10, lane I24W: `-Xverify:none` and `-noverify` print HotSpot
/// 25's deprecation warning, once per occurrence, in the option section
/// only; `--noverify`, `-Xverify:remote|all` and
/// `-XX:-BytecodeVerificationRemote` print nothing.
#[test]
fn t1w10i24w_noverify_deprecation_warning_is_hotspots() {
    const LINE: &str = concat!(
        "OpenJDK 64-Bit Server VM warning: Options -Xverify:none and -noverify ",
        "were deprecated in JDK 13 and will likely be removed in a future release."
    );
    assert_eq!(NOVERIFY_DEPRECATION_WARNING, LINE);
    for spelling in ["-Xverify:none", "-noverify"] {
        assert_eq!(obsolete_flag_warning_of(spelling).as_deref(), Some(LINE), "{spelling}");
    }
    for other in [
        "--noverify",
        "-Xverify:remote",
        "-Xverify:all",
        "-Xverify",
        "-XX:-BytecodeVerificationRemote",
        "-Xverify:none2",
    ] {
        assert!(obsolete_flag_warning_of(other).is_none(), "{other}");
    }
    let line = argv(&["java", "-noverify", "-Xverify:none", "--", "-noverify"]);
    assert_eq!(obsolete_flag_warning_lines(&line, |_| false), [LINE, LINE]);
}

/// Interpreter round i3 wave 79, lane L3: `-Xrs` and
/// `-XX:±ReduceSignalUsage` survive the normaliser (the `-X` catch-all
/// dropped `-Xrs`), the last spelling wins, and absence leaves `None`.
#[test]
fn i79_reduce_signal_usage_is_parsed() {
    let argv0 = argv(&["java", "-XX:-ReduceSignalUsage", "-Xrs", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    assert!(
        stage2.iter().any(|a| a == "-Xrs"),
        "kept verbatim: {stage2:?}"
    );
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, flags) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept -Xrs");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    assert_eq!(flags.reduce_signal_usage, Some(true));
    let (_, off) = extract_hotspot_flags(argv(&["cratonvm", "-XX:-ReduceSignalUsage", "Main"]));
    assert_eq!(off.reduce_signal_usage, Some(false));
    let (_, none) = extract_hotspot_flags(vec!["cratonvm".to_string(), "Main".to_string()]);
    assert!(none.reduce_signal_usage.is_none());
}

#[test]
fn hotspot_server_flag_passes_clap_after_full_pipeline() {
    // WildFly HostController still launches child JVMs with `-server`.
    // HotSpot accepts it as a VM-selection hint; CratonVM ignores it but
    // must not let clap parse it as short flags (`-s -e ...`).
    let argv0: Vec<String> = argv(&["java", "-server", "-classpath", "x", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept HotSpot -server");
    assert_eq!(parsed.classpath.as_deref(), Some("x"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

#[test]
fn hotspot_xss_passes_clap_after_full_pipeline() {
    // B3 acceptance: stock `java -Xss512k -classpath x Main` must parse
    // end-to-end with `Main` resolved as the main class — previously clap
    // aborted on the unrecognized `-Xss512k`.
    let argv0: Vec<String> = argv(&["java", "-Xss512k", "-classpath", "x", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept HotSpot -Xss after pipeline");
    assert_eq!(parsed.classpath.as_deref(), Some("x"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    // gcd d6/f: and the size reaches the launcher, which parses it.
    assert_eq!(parsed.thread_stack_size.as_deref(), Some("512k"));
    assert_eq!(parse_size("512k"), Some(512 * 1024));
    assert_eq!(parse_size("64m"), Some(64 * 1024 * 1024));
}

#[test]
fn user_double_dash_reaches_program_args() {
    // B2 regression: a user `--` between program args
    // (`java Main a -- b`) must be delivered to the program verbatim as
    // `["a", "--", "b"]`. The launcher's own separator is consumed by clap
    // (it never reaches `parsed.args`), so the interior `--` here is purely
    // user data and must survive the whole pipeline.
    let argv0: Vec<String> = argv(&["java", "Main", "a", "--", "b"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse user `--` argv");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    // The interior user `--` is present in the trailing program args.
    assert_eq!(parsed.args, argv(&["a", "--", "b"]));
    // After [LOW arg-parse fix (1)] `run()` no longer pops any trailing
    // `--`; here the last arg is `b` anyway, so the program sees
    // ["a","--","b"] verbatim (the trailing-`--` case is covered by
    // `trailing_user_double_dash_is_preserved_through_pipeline`).
    assert_ne!(parsed.args.last().map(String::as_str), Some("--"));
}

#[test]
fn classpath_class_and_program_args_pipeline() {
    // Regression guard for cli_main_args integration test.
    // `cratonvm --classpath <dir> PrintArgs alpha beta gamma` must produce
    // class_name=PrintArgs and args=["alpha","beta","gamma"].
    let argv0: Vec<String> = argv(&[
        "cratonvm",
        "--classpath",
        "/tmp/dir",
        "PrintArgs",
        "alpha",
        "beta",
        "gamma",
    ]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse classpath+args argv");
    assert_eq!(parsed.class_name.as_deref(), Some("PrintArgs"));
    assert_eq!(parsed.args, argv(&["alpha", "beta", "gamma"]));
}

#[test]
fn launcher_trailing_double_dash_artifact_is_popped() {
    // B2 regression guard: `-mp` is listed in VALUE_TAKING_OPTS, so
    // `insert_program_args_separator` treats `/modules` as `-mp`'s value
    // and does NOT inject a spurious `"--"` between them.
    //
    // The pipeline result for `java -mp /modules`:
    //   normalize inserts `"--"` before `-mp` → clap sees ["java", "--", "-mp", "/modules"]
    //   class_name = Some("-mp")  (first positional after "--")
    //   args = ["/modules"]       (trailing_var_arg gets the rest)
    //
    // The old trailing `"--"` artifact (from when `/modules` was wrongly
    // treated as the main-class name) no longer occurs. The guard pop in
    // `run()` is a no-op.
    let argv0: Vec<String> = argv(&["java", "-mp", "/modules"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse `-mp` argv");
    // `-mp` is consumed as the class_name (first positional after `--`).
    assert_eq!(parsed.class_name.as_deref(), Some("-mp"));
    let mut prog = parsed.args;
    // No trailing "--" artifact — VALUE_TAKING_OPTS prevents the spurious injection.
    assert_ne!(prog.last().map(String::as_str), Some("--"));
    // The guard pop in `run()` is a no-op; args are already clean.
    if prog.last().map(String::as_str) == Some("--") {
        prog.pop();
    }
    // Only `/modules` remains in args; `-mp` went to class_name.
    assert_eq!(prog, argv(&["/modules"]));
}

// -----------------------------------------------------------------------
// [LOW arg-parse fix (1)] Trailing user `--` is preserved as a program arg.
// -----------------------------------------------------------------------

#[test]
fn trailing_user_double_dash_is_preserved_through_pipeline() {
    // `java Main a --` per JDK launcher semantics gives the program
    // `["a", "--"]`. The launcher's own boundary separator is consumed by
    // clap; the interior+trailing user `--` here is genuine program data
    // and the trailing one must NOT be popped (the old unconditional pop in
    // `run()` silently dropped it).
    let argv0: Vec<String> = argv(&["java", "Main", "a", "--"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse trailing `--` argv");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    // The trailing user `--` survives clap as the last program arg.
    assert_eq!(parsed.args.last().map(String::as_str), Some("--"));
    // Mirror `run()`'s post-clap handling: the pop is gone, so the trailing
    // `--` reaches the program verbatim.
    assert_eq!(parsed.args, argv(&["a", "--"]));
}

// -----------------------------------------------------------------------
// [LOW arg-parse fix (2)] A value of a value-taking option that starts with
// `-D` is NOT hijacked as a `-Dkey=value` system property.
// -----------------------------------------------------------------------

#[test]
fn dminus_value_of_value_opt_is_not_a_system_property() {
    // `--Xlog -Dgc` — `-Dgc` is the VALUE of `--Xlog`, not a system
    // property. It must be passed through to clap (as `--Xlog`'s operand)
    // and must NOT appear in the extracted props list.
    let (filtered, props) =
        extract_system_properties(argv(&["java", "--Xlog", "-Dgc", "Main"]));
    assert_eq!(filtered, argv(&["java", "--Xlog", "-Dgc", "Main"]));
    assert!(
        props.is_empty(),
        "value token must not be parsed as -D prop"
    );
}

#[test]
fn dminus_genuine_property_still_extracted_in_option_position() {
    // A genuine `-Dkey=value` in an OPTION position (not following a
    // value-taking option) is still extracted — the fix only protects the
    // value slot, it doesn't disable `-D` handling.
    let (filtered, props) =
        extract_system_properties(argv(&["java", "-Dfoo=bar", "--classpath", "x", "Main"]));
    // `-Dfoo=bar` removed; `--classpath x Main` survive (x is the value of
    // --classpath and is not a -D candidate anyway).
    assert_eq!(filtered, argv(&["java", "--classpath", "x", "Main"]));
    assert_eq!(props, vec![("foo".to_string(), "bar".to_string())]);
}

#[test]
fn dminus_value_starting_with_dminus_for_classpath() {
    // Pathological but legal: a classpath entry literally starting with
    // `-D` (e.g. a directory named `-Dweird`). It is `--classpath`'s value
    // and must survive as-is, not become a fabricated system property.
    let (filtered, props) =
        extract_system_properties(argv(&["java", "--classpath", "-Dweird", "Main"]));
    assert_eq!(filtered, argv(&["java", "--classpath", "-Dweird", "Main"]));
    assert!(props.is_empty());
}

#[test]
fn dminus_after_separator_is_program_arg_not_property() {
    // Unchanged behaviour guard: `-Dfoo=bar` after `--` belongs to the
    // program (jboss-modules style) and is never extracted.
    let (filtered, props) =
        extract_system_properties(argv(&["java", "Main", "--", "-Dfoo=bar"]));
    assert_eq!(filtered, argv(&["java", "Main", "--", "-Dfoo=bar"]));
    assert!(props.is_empty());
}

// -----------------------------------------------------------------------
// [LOW arg-parse fix (3)] An unrecognized `-X` flag must not swallow the
// following main-class token.
// -----------------------------------------------------------------------

#[test]
fn unknown_x_flag_does_not_swallow_following_main_class() {
    // `-Xunknown Main`: the unknown `-X` flag is dropped (`i += 1`), and
    // the following bare token `Main` is NOT consumed as its value — it
    // survives so it can be resolved as the main class.
    let out = normalize_java_launcher_argv(argv(&["java", "-Xunknown", "Main"]));
    assert_eq!(out, argv(&["java", "Main"]));
}

#[test]
fn unknown_x_flag_before_class_resolves_class_through_pipeline() {
    // End-to-end: an unknown separate-looking `-X` flag immediately before
    // the main class must still resolve `Main` (not absorb it). Exercises
    // the full pre-clap pipeline as `run()` would.
    let argv0: Vec<String> = argv(&["java", "-Xint", "-classpath", "x", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed =
        Args::try_parse_from(stage4).expect("clap must accept unknown -X before main class");
    assert_eq!(parsed.classpath.as_deref(), Some("x"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

#[test]
fn unknown_x_flag_directly_before_class_no_classpath() {
    // The minimal swallow scenario with no intervening options:
    // `java -Xint Main` must resolve `Main` as the main class.
    let argv0: Vec<String> = argv(&["java", "-Xint", "Main"]);
    let stage1 = insert_program_args_separator(argv0);
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must accept `-Xint Main`");
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));
}

// -----------------------------------------------------------------------
// Real-JDK mode (docs/book/src/user-guide/real-jdk-mode.md §9)
//
// These pin the launcher half of the contract: the flag surface other
// crates (difftest) already invoke, the conflict *ordering* that decides
// which of two true diagnoses the user sees, the redaction rule, and the
// shape/order of the census files difftest parses.
// -----------------------------------------------------------------------

#[test]
fn jdk_only_flag_surface_matches_the_contract() {
    let parsed = Args::try_parse_from(argv(&[
        "cratonvm",
        "--real-jdk",
        "--real-jdk-report",
        "report.json",
        "--dump-class-origins",
        "origins.json",
        "--dump-native-registry",
        "natives.json",
        "--trace-real-jdk",
        "--explain-real-jdk",
        "Main",
    ]))
    .expect("the seven §9 flags must parse together");
    assert!(parsed.real_jdk);
    assert!(parsed.trace_jdk_only);
    assert!(parsed.explain_jdk_only);
    assert_eq!(parsed.jdk_only_report.as_deref(), Some("report.json"));
    assert_eq!(parsed.dump_class_origins.as_deref(), Some("origins.json"));
    assert_eq!(parsed.dump_native_registry.as_deref(), Some("natives.json"));
    assert_eq!(parsed.class_name.as_deref(), Some("Main"));

    // Conflicts with the opposite POLICY.
    for other in ["--compatible", "--fast-boot"] {
        assert!(
            Args::try_parse_from(argv(&["cratonvm", "--real-jdk", other, "Main"])).is_err(),
            "--real-jdk {other} must be rejected by clap"
        );
    }

    // …and with the other library.
    assert!(
        Args::try_parse_from(argv(&["cratonvm", "--real-jdk", "--synthetic-jdk", "Main"]))
            .is_err(),
        "--real-jdk --synthetic-jdk must be rejected by clap"
    );
    assert!(
        Args::try_parse_from(argv(&[
            "cratonvm",
            "--fast-boot",
            "--synthetic-jdk",
            "Main"
        ]))
        .is_err(),
        "--fast-boot --synthetic-jdk must be rejected by clap"
    );

    // The opt-out parses on its own, and so does the alias and fast-boot.
    let compat = Args::try_parse_from(argv(&["cratonvm", "--compatible", "Main"]))
        .expect("--compatible must parse");
    assert!(compat.fast_boot && !compat.real_jdk);
    let fast_boot = Args::try_parse_from(argv(&["cratonvm", "--fast-boot", "Main"]))
        .expect("--fast-boot must parse");
    assert!(fast_boot.fast_boot && !fast_boot.real_jdk);

    // Both value-taking flags must be known to the pre-clap separator
    // inserter, or their path argument is mistaken for the main class.
    assert!(VALUE_TAKING_OPTS.contains(&"--real-jdk-report"));
    assert!(VALUE_TAKING_OPTS.contains(&"--dump-class-origins"));
    assert!(VALUE_TAKING_OPTS.contains(&"--source"));

    // The long help names every one of them.
    for flag in [
        "--real-jdk",
        "--synthetic-jdk",
        "--compatible",
        "--fast-boot",
        "--real-jdk-report",
        "--dump-class-origins",
        "--trace-real-jdk",
        "--explain-real-jdk",
    ] {
        assert!(LONG_ABOUT.contains(flag), "long_about omits {flag}");
    }
}

/// The banner is printed before clap runs, so `--real-jdk` (and `--real-jdk`) has to be
/// visible to the raw-argv scan — both as "which class library" (it implies
/// real) and as "which policy".
#[test]
fn jdk_only_is_visible_to_the_pre_clap_banner_scan() {
    assert_eq!(
        scan_requested_jdk_mode(&tokens(&["cratonvm", "--real-jdk", "Main", "--"])),
        JdkMode::Real
    );
    assert_eq!(
        scan_requested_compatibility_mode(&tokens(&["cratonvm", "--real-jdk", "Main", "--"])),
        CompatibilityMode::JdkOnly
    );
    assert_eq!(
        scan_requested_jdk_mode(&tokens(&["cratonvm", "--real-jdk", "Main", "--"])),
        JdkMode::Real
    );
    assert_eq!(
        scan_requested_compatibility_mode(&tokens(&["cratonvm", "--real-jdk", "Main", "--"])),
        CompatibilityMode::JdkOnly
    );
    // A program argument is not a launcher flag -- so this run lands on
    // the LAUNCHER DEFAULT, which is strict (`--real-jdk`).
    assert_eq!(
        scan_requested_compatibility_mode(&tokens(&["cratonvm", "Main", "--", "--real-jdk"])),
        cratonvm_vm::config::LAUNCHER_DEFAULT_COMPATIBILITY_MODE
    );
    assert_eq!(
        scan_requested_compatibility_mode(&tokens(&["cratonvm", "Main"])),
        CompatibilityMode::JdkOnly,
        "a bare run is strict now"
    );
    // The compatible-side spellings, including the alias and the implied
    // one. Each must beat the default, and `--compatible` must beat a
    // `--real-jdk` appearing alongside it, mirroring the authoritative
    // resolver's early return (the pair is an error there; the banner is
    // a report, not a gate).
    for argv in [
        vec!["cratonvm", "--compatible", "Main"],
        vec!["cratonvm", "--fast-boot", "Main"],
        vec!["cratonvm", "--synthetic-jdk", "Main"],
        vec!["cratonvm", "--real-jdk", "--compatible", "Main"],
        vec!["cratonvm", "--real-jdk", "--fast-boot", "Main"],
        vec!["cratonvm", "--real-jdk", "--compatible", "Main"],
        vec!["cratonvm", "--real-jdk", "--fast-boot", "Main"],
    ] {
        assert_eq!(
            scan_requested_compatibility_mode(&tokens(&argv)),
            CompatibilityMode::Compatible,
            "{argv:?}"
        );
    }

    let strict = version_banner(
        VersionQuery::Version,
        JdkMode::Real,
        CompatibilityMode::JdkOnly,
        None,
    );
    assert!(
        strict.contains("compatibility=jdk-only"),
        "the build line must name the policy: {strict}"
    );
    // ...and the execution-mode list must be the one `java.vm.info` will
    // carry, so the banner and the property agree on the policy token.
    assert!(
        strict.contains(cratonvm_vm::vm::vm_info_mode_list(
            CompatibilityMode::JdkOnly
        )),
        "the banner must spell the mode list the way java.vm.info does: {strict}"
    );
    let default = version_banner(
        VersionQuery::Version,
        JdkMode::Real,
        CompatibilityMode::Compatible,
        None,
    );
    assert!(default.contains("compatibility=compatible"), "{default}");
    // Compatible mode's banner is what it always was (§10), less the
    // `sharing` claim (gci i33/ms: no CDS archive).
    assert!(default.contains("(build ") && default.contains(", mixed mode, compatibility=compatible)"), "{default}");
    assert!(!default.contains("sharing"), "{default}");
    assert!(!default.contains("jdk-only"), "{default}");
}

/// Ordering matters: `--real-jdk --synthetic-jdk` is a policy conflict, and
/// resolving the policy before the library is what makes the user see that
/// diagnosis rather than the generic library one.
#[test]
fn compatibility_resolution_reports_the_policy_conflict() {
    // (real_jdk, fast_boot, synthetic)
    let mode = |a, b, c| resolve_compatibility_mode(a, b, c);

    assert_eq!(
        mode(false, false, false).expect("a bare run resolves"),
        CompatibilityMode::JdkOnly,
        "the launcher default is strict"
    );
    assert_eq!(
        mode(true, false, false).expect("--real-jdk alone is fine"),
        CompatibilityMode::JdkOnly
    );
    assert_eq!(
        mode(false, true, false).expect("--fast-boot alone is fine"),
        CompatibilityMode::Compatible
    );
    // `--synthetic-jdk` IMPLIES compatible rather than inheriting the
    // default. Inheriting it would make a bare `--synthetic-jdk` a
    // `validate_compatibility` error, caused by a change to a flag it has
    // nothing to do with.
    assert_eq!(
        mode(false, false, true).expect("--synthetic-jdk alone is fine"),
        CompatibilityMode::Compatible
    );

    let err = mode(true, false, true).expect_err("the pair must be rejected");
    let msg = format!("{err:#}");
    assert!(msg.contains("--real-jdk"), "{msg}");
    assert!(msg.contains("--synthetic-jdk"), "{msg}");
    // The specific (policy) diagnosis, not `resolve_jdk_mode`'s generic
    // "two different standard-library implementations" one.
    assert!(!msg.contains("mutually exclusive"), "{msg}");

    let err = mode(true, true, false).expect_err("--real-jdk --fast-boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("--real-jdk") && msg.contains("--fast-boot"),
        "{msg}"
    );
}

// The census writers, the JSON helpers, the origin-bucket fold and the
// path redaction all moved to `vm/src/vm/vm_init.rs` when the two
// implementations were collapsed to one; their tests moved with them
// (`cargo test -p cratonvm-vm`). What stays testable here is the launcher's
// own job: flag parsing, mode resolution, and the banner.

// gc-common w4-f: both exit arms print the TTSP line through ONE
// formatter, and it names the slowest pause's straggler.
#[test]
fn w4f_ttsp_exit_line_names_the_straggler() {
    use cratonvm_vm::threading::gc_barrier::TtspStats;
    let none = ttsp_exit_line(&TtspStats::default());
    assert_eq!(
        none,
        "[GC] ttsp: pauses=0 avg_us=0 max_us=0 last_us=0 max_straggler=none"
    );
    let some = ttsp_exit_line(&TtspStats {
        pauses: 2,
        total_ns: 3_000_000,
        max_ns: 2_000_000,
        last_ns: 1_000_000,
        max_straggler: Some(7),
        last_straggler: None,
    });
    assert_eq!(
        some,
        "[GC] ttsp: pauses=2 avg_us=1500 max_us=2000 last_us=1000 max_straggler=7"
    );
}

// gc-common w5-f (`handoff-w4a-print-the-blocked-fold-census`): the
// blocked-thread fold's share of the root fix-up, silent when no fold ran.
#[test]
fn w5f_blocked_fold_exit_line_is_silent_without_folds_and_means_per_fold() {
    assert_eq!(blocked_fold_exit_line(0, 0, 0), None);
    assert_eq!(blocked_fold_exit_line(0, 3, 9_000), None);
    assert_eq!(
        blocked_fold_exit_line(4, 12, 6_000_000).as_deref(),
        Some("[cratonvm] root fix-up blocked_fold: folds=4 threads=12 total=6ms mean=1500us")
    );
}

// -----------------------------------------------------------------------
// gc-common w11-f — the error exit leaves from the launcher thread
// (`common-w10v-uncaught-error-exit-can-hang-in-process-exit`).
// -----------------------------------------------------------------------

/// Selects the child role of the two re-exec tests below. Not `CRATONVM_*`:
/// it is test plumbing, read only by this module, never by the VM.
const W11F_EXIT_CHILD_ENV: &str = "W11F_EXIT_CHILD_ROLE";

/// The lock a "collector" thread holds almost all the time, and which the
/// "main-vm" thread's thread-local destructor takes. It stands in for
/// `card_table::BUFFER_REGISTRY` / a `SatbBufferGuard`'s buffer lock.
static W11F_COLLECTOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static W11F_COLLECTOR_BUSY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A per-thread guard shaped like `DirtyBufferGuard` / `SatbBufferGuard`:
/// its `Drop` takes a lock that a collector thread also takes.
struct W11fBarrierBufferGuard;
impl Drop for W11fBarrierBufferGuard {
    fn drop(&mut self) {
        let _held = W11F_COLLECTOR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}
thread_local! {
    static W11F_GUARD: W11fBarrierBufferGuard = const { W11fBarrierBufferGuard };
}

/// Child body: a busy "collector" thread, a "main-vm" thread that has
/// touched the guard and ends on the error arm with code 1, and then the
/// exit. `vm_thread_exits_itself` selects the pre-w11-f shape
/// (`std::process::exit(1)` on the `main-vm` thread) for the control test.
fn w11f_exit_child(vm_thread_exits_itself: bool) -> ! {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    // Never ends: the process exit kills it, as it kills G1's workers.
    std::thread::Builder::new()
        .name("w11f-collector".into())
        .spawn(|| loop {
            let held = W11F_COLLECTOR_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            W11F_COLLECTOR_BUSY.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(20));
            W11F_COLLECTOR_BUSY.store(false, Ordering::Release);
            drop(held);
            std::thread::sleep(Duration::from_millis(2));
        })
        .expect("spawn the collector stand-in");
    let vm = std::thread::Builder::new()
        .name("main-vm".into())
        .spawn(move || -> i32 {
            W11F_GUARD.with(|_| {});
            // End while the collector is inside its critical section, as a
            // busy G1 worker almost always is.
            while !W11F_COLLECTOR_BUSY.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            if vm_thread_exits_itself {
                std::process::exit(1);
            }
            1
        })
        .expect("spawn the main-vm stand-in");
    exit_from_launcher(join_main_vm_thread(vm));
    // `exit_from_launcher` returns only for code 0, which this child never
    // produces. Exit with a code the parent does not accept.
    std::process::exit(97);
}

/// Re-run this test binary as a child in `role`, filtered to `test`, and
/// wait at most `limit` for it. `None` means it had not exited by then (it
/// was killed).
fn w11f_run_exit_child(
    test: &str,
    role: &str,
    limit: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let mut child = std::process::Command::new(exe)
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(W11F_EXIT_CHILD_ENV, role)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("re-exec the test binary");
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                // `Child::kill` is `TerminateProcess` on Windows, so this
                // ends a hung child for certain.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The fix: a VM whose `main-vm` thread ends on the error arm while a
/// collector thread is busy exits with code 1, promptly. The launcher
/// joins the thread (its thread-local destructors run while the collector
/// is still alive and so can release its lock) and only then exits.
///
/// Bounded: the child needs well under a second; 60 s allows for a loaded
/// host running this binary's other tests in parallel.
#[test]
fn w11f_error_exit_with_a_busy_collector_exits_from_the_launcher() {
    if std::env::var(W11F_EXIT_CHILD_ENV).as_deref() == Ok("launcher") {
        w11f_exit_child(false);
    }
    let status = w11f_run_exit_child(
        "tests::w11f_error_exit_with_a_busy_collector_exits_from_the_launcher",
        "launcher",
        std::time::Duration::from_secs(60),
    )
    .expect(
        "the error exit hung: the process did not end within 60 s while a collector \
         thread was busy (common-w10v-uncaught-error-exit-can-hang-in-process-exit)",
    );
    assert_eq!(
        status.code(),
        Some(1),
        "the error arm must exit with code 1 (a 97 means `exit_from_launcher` returned)"
    );
}

/// CONTROL, ignored by default because on Windows it is expected to hang
/// for its whole 10 s bound: the pre-w11-f shape, where the `main-vm`
/// thread itself calls `std::process::exit(1)`. `ExitProcess` kills the
/// busy collector inside its critical section, then runs the `main-vm`
/// thread's thread-local destructor, which waits for that lock for ever.
/// Run it with `--ignored` to confirm the mechanism on a host: a timeout
/// here and a pass above is the evidence. On other hosts `exit` kills no
/// thread first, so the child exits normally and the test says so.
#[test]
#[ignore = "control: demonstrates the pre-w11-f hang on Windows (10 s)"]
fn w11f_control_exit_on_the_vm_thread_hangs_on_windows() {
    if std::env::var(W11F_EXIT_CHILD_ENV).as_deref() == Ok("vm-thread") {
        w11f_exit_child(true);
    }
    let status = w11f_run_exit_child(
        "tests::w11f_control_exit_on_the_vm_thread_hangs_on_windows",
        "vm-thread",
        std::time::Duration::from_secs(10),
    );
    if cfg!(windows) {
        assert!(
            status.is_none(),
            "expected the pre-w11-f exit shape to hang on Windows, but it exited with \
             {status:?}; the w11-f diagnosis (thread-local destructors after \
             ExitProcess killed the lock holder) is then not the mechanism on this host"
        );
    } else {
        assert_eq!(status.and_then(|s| s.code()), Some(1));        }    }

/// The panic arm keeps its code: a `main-vm` thread that panicked is exit
/// code 1, and a returned code passes through unchanged.
#[test]
fn w11f_join_main_vm_thread_maps_a_panic_to_one_and_passes_codes_through() {
    let returned = std::thread::spawn(|| -> i32 { 1 });
    assert_eq!(join_main_vm_thread(returned), 1);
    let ok = std::thread::spawn(|| -> i32 { 0 });
    assert_eq!(join_main_vm_thread(ok), 0);
    let panicked =
        std::thread::spawn(|| -> i32 { panic!("w11f: the main-vm thread panicked") });
    assert_eq!(join_main_vm_thread(panicked), 1);
    // Code 0 returns, so `main` returns exactly as the normal exit does.
    exit_from_launcher(0);
}

/// `dumponexit` plumbing shared by every exit arm: no request, no dump; a
/// request is attempted and its failure reported rather than raised. (The
/// successful dump of a running recording is covered by
/// `vm/tests/tier1_tests.rs::d12_dump_recording_via_the_stashed_exit_target_produces_a_real_jfr_file`;
/// it is not repeated here because starting a recording flips the
/// process-wide JFR gate for every other test in this binary.)
#[test]
fn w11f_jfr_exit_dump_is_attempted_only_when_requested() {
    let shared = cratonvm_vm::SharedVm::new(cratonvm_vm::config::VmConfig::default());
    assert!(write_jfr_exit_recording(&shared).is_none());
    let path = std::env::temp_dir().join(format!(
        "cratonvm-w11f-no-such-recording-{}.jfr",
        std::process::id()
    ));
    *shared.debug.jfr_dump_on_exit.lock() =
        Some((u64::MAX, path.to_string_lossy().into_owned()));
    match write_jfr_exit_recording(&shared) {
        Some(Err(message)) => assert!(!message.is_empty()),
        other => panic!("an unknown recording must report an error, got {other:?}"),
    }
    assert!(
        !path.exists(),
        "no file may be written for an unknown recording"
    );
}

/// gc-common w12-e: the launcher waits for the non-daemon threads when
/// `main` threw, as HotSpot's `DestroyJavaVM` does, not only when it
/// returned. An internal VM failure keeps the historical no-wait exit.
/// The end-to-end half is `vm-cli/tests/cli_uncaught_nondaemon.rs`.
#[test]
fn w12e_the_launcher_waits_for_non_daemon_threads_when_main_threw() {
    // Never dereferenced: `MainEnding::of` only looks at the variant.
    let mut word = 0u64;
    let throwable =
        unsafe { cratonvm_types::ObjectRef::from_raw(&mut word as *mut u64 as *mut u8) };
    let returned: std::result::Result<Option<Value>, MethodCallFailed> = Ok(None);
    let threw: std::result::Result<Option<Value>, MethodCallFailed> =
        Err(MethodCallFailed::ExceptionThrown(throwable));
    let failed: std::result::Result<Option<Value>, MethodCallFailed> = Err(
        MethodCallFailed::InternalError(cratonvm_vm::error::VmError::Internal {
            message: "w12e".into(),
        }),
    );
    assert_eq!(MainEnding::of(&returned), MainEnding::Returned);
    assert_eq!(MainEnding::of(&threw), MainEnding::Threw);
    assert_eq!(MainEnding::of(&failed), MainEnding::InternalError);
    assert!(MainEnding::Returned.waits_for_non_daemon_threads());
    assert!(
        MainEnding::Threw.waits_for_non_daemon_threads(),
        "HotSpot's JavaMain reaches DestroyJavaVM after an uncaught exception too"
    );
    assert!(!MainEnding::InternalError.waits_for_non_daemon_threads());
}

/// gcf f1/r: the registry's OUTERMOST-first trace is reported innermost
/// first, in `StackTraceElement.toString()`'s shape, as HotSpot prints
/// `TraceOrder` (inner, outer, main).
#[test]
fn gcf_f1r_uncaught_report_lists_frames_innermost_first() {
    let frame = |class: &str, method: &str, file: Option<&str>, line: i32| {
        cratonvm_vm::StackTraceFrame {
            class: class.to_string(),
            method: method.to_string(),
            file: file.map(str::to_string),
            line,
        }
    };
    // Outermost first, as `Vm::throwable_stack_for` returns it.
    let stored = [
        frame("TraceOrder", "main", Some("TraceOrder.java"), 4),
        frame("TraceOrder", "outer", Some("TraceOrder.java"), 3),
        frame("pkg/Inner", "inner", Some("Inner.java"), -1),
        frame("pkg/Nat", "call", Some("Nat.java"), -2),
        frame("pkg/NoSrc", "x", None, 7),
    ];
    assert_eq!(
        uncaught_report_frame_lines(&stored),
        [
            "\tat pkg.NoSrc.x(Unknown Source)",
            "\tat pkg.Nat.call(Native Method)",
            "\tat pkg.Inner.inner(Inner.java)",
            "\tat TraceOrder.outer(TraceOrder.java:3)",
            "\tat TraceOrder.main(TraceOrder.java:4)",
        ]
    );
    assert_eq!(
        binary_class_name("java/lang/IllegalStateException"),
        "java.lang.IllegalStateException"
    );
}

/// gcf f1/r: a `Caused by:` prints its frames up to the trailing run it
/// shares with the enclosing trace (`Throwable.printEnclosedStackTrace`).
#[test]
fn gcf_f1r_a_cause_elides_the_frames_in_common() {
    let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let outer = t(&[
        "\tat A.wrap(A.java:9)",
        "\tat A.outer(A.java:5)",
        "\tat A.main(A.java:2)",
    ]);
    let cause = t(&[
        "\tat A.inner(A.java:7)",
        "\tat A.outer(A.java:5)",
        "\tat A.main(A.java:2)",
    ]);
    assert_eq!(frames_not_in_common(&outer, None), 3);
    let none: Vec<String> = Vec::new();
    assert_eq!(frames_not_in_common(&cause, Some(outer.as_slice())), 1);
    assert_eq!(frames_not_in_common(&outer, Some(outer.as_slice())), 0);
    assert_eq!(frames_not_in_common(&cause, Some(none.as_slice())), 3);
    assert_eq!(frames_not_in_common(&none, Some(outer.as_slice())), 0);
}

/// gcf f2/r: the text `printStackTrace(PrintWriter)` wrote becomes the
/// report verbatim, with `ThreadGroup.uncaughtException`'s header prefix
/// on the first line and the platform line separator dropped.
#[test]
fn gcf_f2r_a_printed_trace_becomes_the_report_with_the_thread_header() {
    let crlf = "java.lang.IllegalStateException: x\r\n\
                \tat java.base/java.lang.Thread.join(Thread.java:2072)\r\n\
                \tat T.main(T.java:3)\r\n\
                \tSuppressed: java.lang.IllegalStateException: close\r\n\
                \t\tat T.lambda$main$0(T.java:3)\r\n\
                \t\t... 1 more\r\n";
    let lines = uncaught_report_lines_from_printed_trace(crlf, "main").expect("a header");
    assert_eq!(
        lines,
        vec![
            "Exception in thread \"main\" java.lang.IllegalStateException: x",
            "\tat java.base/java.lang.Thread.join(Thread.java:2072)",
            "\tat T.main(T.java:3)",
            "\tSuppressed: java.lang.IllegalStateException: close",
            "\t\tat T.lambda$main$0(T.java:3)",
            "\t\t... 1 more",
        ]
    );
    let lf = "java.lang.Error\n";
    assert_eq!(
        uncaught_report_lines_from_printed_trace(lf, "main"),
        Some(vec![
            "Exception in thread \"main\" java.lang.Error".to_string()
        ])
    );
}

/// gcf f2/r: nothing printed (or only separators) is not a report; the
/// caller falls back to the Rust renderer.
#[test]
fn gcf_f2r_an_empty_printed_trace_falls_back() {
    assert_eq!(uncaught_report_lines_from_printed_trace("", "main"), None);
    assert_eq!(
        uncaught_report_lines_from_printed_trace("\r\n\n", "main"),
        None
    );
    assert_eq!(
        uncaught_report_lines_from_printed_trace("\n\tat A.b(A.java:1)\n", "main"),
        None
    );
}

/// nio21 w9 L4: the header names the thread the way `Thread.getName()`
/// answers, so a renamed main thread is reported under its new name
/// (`Nio21UncaughtMain renamed`).
#[test]
fn nio21_w9_l4_the_printed_trace_header_names_the_thread() {
    assert_eq!(
        uncaught_report_lines_from_printed_trace("java.lang.Error: x\n", "renamed-main"),
        Some(vec![
            "Exception in thread \"renamed-main\" java.lang.Error: x".to_string()
        ])
    );
}

/// nio21 w8 L4: an uncaught exception whose report is already out reaches
/// the `main-vm` error arm as `UncaughtReported`, which that arm must be
/// able to recognise through `anyhow` (it prints nothing for it); every
/// other error is still a plain `anyhow` error the arm prefixes.
#[test]
fn nio21_w8_l4_uncaught_reported_survives_anyhow() {
    let reported: anyhow::Error = UncaughtReported("ReportedByThreadGroup".to_string()).into();
    assert!(reported.downcast_ref::<UncaughtReported>().is_some());
    assert_eq!(
        reported.chain().count(),
        1,
        "no cause chain, so no Debug line either"
    );
    assert!(format!("{reported:#}").contains("ReportedByThreadGroup"));
    let other = anyhow::anyhow!("Error in thread \"main\" internal error");
    assert!(other.downcast_ref::<UncaughtReported>().is_none());
}

/// nio21 w8 L4: the default `tracing` floor is ERROR (HotSpot's stderr has
/// no WARN events), and an explicit global `RUST_LOG` still replaces it.
#[test]
fn nio21_w8_l4_default_log_floor_is_error() {
    assert_eq!(DEFAULT_LOG_FLOOR, tracing::Level::ERROR);
    assert!(rust_log_names_a_global_level(Some("warn")));
    assert!(!rust_log_names_a_global_level(None));
}
