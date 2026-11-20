use super::*;

#[test]
fn an_empty_batch_compile_never_spawns_javac() {
    // Safe on a worker with no JDK: the matrix subcommand calls this before
    // it knows whether the corpus contains any sources at all.
    assert!(compile_java_batch(&[], Path::new("."), None, Duration::from_secs(1)).is_ok());
}

#[test]
fn parse_modes_round_trips_labels() {
    let modes = parse_modes("jit-on, nojit ,no-intrinsics").unwrap();
    assert_eq!(modes, vec![Mode::JitOn, Mode::NoJit, Mode::NoIntrinsics]);
    let labels: Vec<_> = modes.iter().map(|m| m.label()).collect();
    assert_eq!(labels, vec!["jit-on", "nojit", "no-intrinsics"]);
}

#[test]
fn parse_modes_rejects_unknown() {
    let err = parse_modes("jit-on,bogus").unwrap_err();
    assert!(err.contains("bogus"), "{err}");
}

#[test]
fn parse_modes_skips_empties() {
    assert!(parse_modes("").unwrap().is_empty());
    assert_eq!(parse_modes("nojit,").unwrap(), vec![Mode::NoJit]);
}

#[test]
fn nojit_sets_disable_jit_env() {
    assert_eq!(
        Mode::NoJit.env_overrides(),
        &[("CRATONVM_DISABLE_JIT", "1")]
    );
    assert!(Mode::JitOn.env_overrides().is_empty());
}

#[test]
fn all_mode_knobs_cover_every_override() {
    // The clear-list must be a superset of every mode's set-keys, or an
    // inherited knob could leak into a mode that doesn't set it.
    for mode in Mode::all() {
        for (k, _) in mode.env_overrides() {
            assert!(
                ALL_MODE_KNOBS.contains(k),
                "{k} missing from ALL_MODE_KNOBS"
            );
        }
    }
}

// -- the historical five are frozen -------------------------------------

/// The five modes the committed `difftest/seeds` gate baseline was captured
/// with. Their labels, argv and env are a compatibility surface.
const LEGACY_MODES: [Mode; 5] = [
    Mode::JitOn,
    Mode::NoJit,
    Mode::NoIntrinsics,
    Mode::MovingGc,
    Mode::LowJitThreshold,
];

#[test]
fn legacy_modes_run_the_compatible_policy_and_no_census() {
    // The whole point of the new modes being *new*: adding a policy axis
    // must not change what the historical five MEASURE. A dump leaking in
    // here silently re-measures the CI baseline.
    //
    // Until 2026-09-20 "measures the same thing" and "passes no launcher
    // flag" were the same assertion, and this test made the second one.
    // The launcher default flipped that day, so they came apart: an empty
    // argv now selects `jdk-only`. The invariant that actually matters is
    // the policy, so that is what is asserted — `--compatible` and nothing
    // else, which reproduces the VM behaviour these baselines hold.
    for mode in LEGACY_MODES {
        assert_eq!(
            mode.cli_args(),
            &["--compatible"],
            "{} must state the compatible policy and pass nothing else",
            mode.label()
        );
        assert!(
            !mode.collects_census(),
            "{} must not request census dumps",
            mode.label()
        );
        assert!(!mode.is_jdk_only(), "{} is not strict", mode.label());
        assert_eq!(mode.jdk_profile(), "compatible", "{}", mode.label());
    }
}

#[test]
fn legacy_labels_and_env_are_unchanged() {
    let labels: Vec<&str> = LEGACY_MODES.iter().map(|m| m.label()).collect();
    assert_eq!(
        labels,
        vec![
            "jit-on",
            "nojit",
            "no-intrinsics",
            "moving-gc",
            "low-jit-threshold"
        ]
    );
    assert!(Mode::JitOn.env_overrides().is_empty());
    assert_eq!(
        Mode::NoJit.env_overrides(),
        &[("CRATONVM_DISABLE_JIT", "1")]
    );
    assert_eq!(
        Mode::NoIntrinsics.env_overrides(),
        &[("CRATONVM_DISABLE_INTRINSICS", "1")]
    );
    assert_eq!(
        Mode::MovingGc.env_overrides(),
        &[("CRATONVM_NO_SELECTIVE_PROMOTE", "1")]
    );
    assert_eq!(
        Mode::LowJitThreshold.env_overrides(),
        &[("CRATONVM_JIT_THRESHOLD", "1")]
    );
    // The default `--modes` list is still the historical pair.
    assert_eq!(
        RunnerConfig::for_corpus(PathBuf::from("x")).modes,
        vec![Mode::JitOn, Mode::NoJit]
    );
}

// -- the profile modes ---------------------------------------------------

#[test]
fn profile_modes_round_trip_and_carry_one_policy_flag() {
    let modes =
        parse_modes("jdk-only-jit,jdk-only-nojit,real-compatible-jit,real-compatible-nojit")
            .unwrap();
    assert_eq!(
        modes,
        vec![
            Mode::JdkOnlyJit,
            Mode::JdkOnlyNoJit,
            Mode::RealCompatibleJit,
            Mode::RealCompatibleNoJit
        ]
    );
    for m in &modes {
        assert!(m.collects_census(), "{} collects a census", m.label());
        assert_eq!(m.cli_args().len(), 1, "{}", m.label());
        assert_eq!(Mode::from_label(m.label()), Some(*m), "label round-trips");
    }
    assert_eq!(Mode::JdkOnlyJit.cli_args(), &["--real-jdk"]);
    assert_eq!(Mode::RealCompatibleNoJit.cli_args(), &["--fast-boot"]);
    // The nojit half of each pair disables the JIT the same way `nojit` does.
    assert_eq!(
        Mode::JdkOnlyNoJit.env_overrides(),
        &[("CRATONVM_DISABLE_JIT", "1")]
    );
    assert!(Mode::JdkOnlyJit.env_overrides().is_empty());
}

#[test]
fn every_mode_states_its_policy_and_never_both() {
    // Two properties, and the second one is why the first exists.
    //
    // (1) No mode passes both policies: the launcher refuses that argv
    //     outright, and before it did, the verdict would have been decided
    //     by argument order.
    // (2) EVERY mode passes one. Since 2026-09-20 an empty argv means
    //     `jdk-only`, so a mode that stated nothing would silently run
    //     strict while `jdk_profile()` kept labelling it `compatible` —
    //     every historical ledger row relabelled without being remeasured.
    for &m in Mode::all() {
        let args = m.cli_args();
        let strict = args.contains(&"--real-jdk");
        let compatible = args.contains(&"--compatible") || args.contains(&"--fast-boot");
        assert!(
            strict ^ compatible,
            "{} must state exactly one policy, got {args:?}",
            m.label()
        );
        assert_eq!(
            strict,
            m.is_jdk_only(),
            "{} disagrees with is_jdk_only()",
            m.label()
        );
        assert_eq!(
            m.jdk_profile(),
            if strict { "jdk-only" } else { "compatible" },
            "{} ledger profile disagrees with its argv",
            m.label()
        );
    }
}

#[test]
fn profile_maps_to_the_compatibility_mode_spelling() {
    assert_eq!(Mode::JdkOnlyJit.jdk_profile(), "jdk-only");
    assert_eq!(Mode::JdkOnlyNoJit.jdk_profile(), "jdk-only");
    // `real-compatible-*` shares the historical five's profile: it differs
    // in observability, not policy, so it keys onto the same ledger rows.
    assert_eq!(Mode::RealCompatibleJit.jdk_profile(), "compatible");
    assert_eq!(Mode::RealCompatibleNoJit.jdk_profile(), "compatible");
    assert!(!Mode::RealCompatibleJit.is_jdk_only());
}

#[test]
fn all_lists_every_mode_exactly_once() {
    let labels: Vec<&str> = Mode::all().iter().map(|m| m.label()).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len(), "duplicate label in Mode::all()");
    assert_eq!(labels.len(), 15, "5 legacy + 4 profile + 6 execution-path");
    for label in &labels {
        assert!(Mode::from_label(label).is_some(), "{label} must parse");
    }
}

// -- the execution-path modes --------------------------------------------

#[test]
fn execution_path_modes_round_trip_and_stay_on_the_compatible_profile() {
    let modes =
        parse_modes("interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt").unwrap();
    assert_eq!(
        modes,
        vec![
            Mode::InterpDecoded,
            Mode::DirectEmit,
            Mode::IrJit,
            Mode::OsrEager,
            Mode::NoOsr,
            Mode::ForcedDeopt
        ]
    );
    for m in &modes {
        assert_eq!(Mode::from_label(m.label()), Some(*m), "label round-trips");
        assert!(m.is_execution_path(), "{}", m.label());
        // They are an execution axis, not a policy axis: they must key onto
        // the same ledger rows the historical five do, or a JIT-path
        // finding would silently fork a second baseline.
        assert_eq!(m.jdk_profile(), crate::ledger::PROFILE_COMPATIBLE);
        assert!(!m.is_jdk_only());
        assert!(!m.collects_census(), "{} must not request dumps", m.label());
    }
    // The historical and profile modes are not execution-path rows.
    for m in [Mode::JitOn, Mode::NoJit, Mode::JdkOnlyJit, Mode::MovingGc] {
        assert!(!m.is_execution_path(), "{}", m.label());
    }
}

#[test]
fn execution_path_modes_set_the_flags_their_docs_name() {
    // Each of these was verified against a live read site; the assertion is
    // here so a rename in the VM shows up as a difftest failure rather than
    // as a mode that silently stops selecting anything. The read sites are
    // listed on each variant and in docs/testing/differential.md.
    assert_eq!(
        Mode::InterpDecoded.cli_args(),
        &["--compatible", "--noverify"]
    );
    assert_eq!(
        Mode::InterpDecoded.env_overrides(),
        &[("CRATONVM_DISABLE_JIT", "1")]
    );
    assert!(Mode::DirectEmit
        .env_overrides()
        .contains(&("CRATONVM_NO_IR_BRANCHY", "1")));
    assert!(Mode::DirectEmit
        .env_overrides()
        .contains(&("CRATONVM_JIT_IR_CALL", "0")));
    assert!(Mode::IrJit
        .env_overrides()
        .contains(&("CRATONVM_JIT_FORCE_C2", "1")));
    assert!(Mode::OsrEager
        .env_overrides()
        .contains(&("CRATONVM_TIER_OSR_BACKEDGE", "1")));
    assert_eq!(Mode::NoOsr.env_overrides(), &[("CRATONVM_JIT_OSR", "0")]);
    assert!(Mode::ForcedDeopt
        .env_overrides()
        .contains(&("CRATONVM_DEOPT_EAGER", "1")));
    assert!(Mode::ForcedDeopt
        .env_overrides()
        .contains(&("CRATONVM_DEOPT_VERIFY", "1")));
    // Only `interp-decoded` adds a launcher flag of its own; the rest are
    // env-only, so they cannot perturb argv-sensitive behaviour. They all
    // carry `--compatible` since 2026-09-20 — that is not a knob of
    // theirs, it is the policy the launcher stopped defaulting to, and it
    // is identical across every mode here, so it still cannot make two of
    // them differ.
    for m in Mode::execution_paths() {
        let extra: Vec<&str> = m
            .cli_args()
            .iter()
            .copied()
            .filter(|a| *a != "--compatible")
            .collect();
        let expected: &[&str] = if *m == Mode::InterpDecoded {
            &["--noverify"]
        } else {
            &[]
        };
        assert_eq!(extra, expected, "{}", m.label());
    }
}

#[test]
fn osr_pair_differs_in_exactly_one_bit() {
    // The value of the pair is that a divergence between them isolates OSR
    // with no reference JDK involved — which only holds if nothing else
    // differs. `osr-eager` additionally lowers the trigger; the assertion
    // is that the master enable has opposite senses and no third knob
    // appears in `no-osr`.
    let eager: Vec<&str> = Mode::OsrEager
        .env_overrides()
        .iter()
        .map(|(k, _)| *k)
        .collect();
    assert!(eager.contains(&"CRATONVM_JIT_OSR"));
    assert_eq!(Mode::NoOsr.env_overrides().len(), 1);
}

#[test]
fn execution_paths_lists_the_interpreter_fast_row_first() {
    // `nojit` *is* the interpreter-fast row (raw superinstruction handlers,
    // no JIT), so the contract list must lead with it — otherwise the
    // decoded fallback has nothing to be compared against.
    assert_eq!(Mode::execution_paths()[0], Mode::NoJit);
    assert!(Mode::execution_paths().contains(&Mode::InterpDecoded));
}

// -- census dumps --------------------------------------------------------

#[test]
fn dump_paths_emit_the_three_contract_flags() {
    let dumps = DumpPaths::tagged(Path::new("scratch"), "Foo-jdk-only-jit");
    let args = dumps.cli_args();
    assert_eq!(args.len(), 6);
    assert_eq!(args[0], "--real-jdk-report");
    assert_eq!(args[2], "--dump-class-origins");
    assert_eq!(args[4], "--dump-native-registry");
    assert!(dumps
        .jdk_only_report
        .ends_with("Foo-jdk-only-jit-jdk-only-report.json"));
    // Distinct tags never collide in one scratch dir.
    let other = DumpPaths::tagged(Path::new("scratch"), "Foo-real-compatible-jit");
    assert_ne!(dumps.jdk_only_report, other.jdk_only_report);
}

#[test]
fn dump_cleanup_removes_stale_files() {
    let dir = std::env::temp_dir().join(format!("difftest_dumps_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dumps = DumpPaths::in_dir(&dir);
    std::fs::write(&dumps.jdk_only_report, "{}").expect("write");
    assert!(dumps.jdk_only_report.exists());
    // A stale report from a previous run must not be readable as this
    // run's measurement.
    dumps.cleanup();
    assert!(!dumps.jdk_only_report.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// -- JDK feature probe ---------------------------------------------------

#[test]
fn parses_the_modern_version_banner() {
    assert_eq!(
        parse_jdk_feature("openjdk version \"25.0.3\" 2026-04-21 LTS\nOpenJDK Runtime ...\n"),
        Some(25)
    );
    assert_eq!(parse_jdk_feature("openjdk version \"17\"\n"), Some(17));
    assert_eq!(
        parse_jdk_feature("openjdk version \"21.0.2\" 2024-01-16"),
        Some(21)
    );
    assert_eq!(
        parse_jdk_feature("openjdk version \"24-ea\" 2025-03-18"),
        Some(24)
    );
}

#[test]
fn parses_the_legacy_1_8_banner() {
    // Pre-9 JDKs spell 8 as `1.8.0_x`; reading the leading component would
    // record every JDK 8 run as feature 1.
    assert_eq!(parse_jdk_feature("java version \"1.8.0_412\"\n"), Some(8));
    assert_eq!(parse_jdk_feature("java version \"1.7.0_80\"\n"), Some(7));
}

#[test]
fn an_unreadable_banner_is_none_not_zero() {
    assert_eq!(parse_jdk_feature(""), None);
    assert_eq!(parse_jdk_feature("no version here\n"), None);
    assert_eq!(parse_jdk_feature("openjdk version \"weird\"\n"), None);
}

#[test]
fn java_executable_honors_explicit_home() {
    let home = Path::new("/opt/jdk-25");
    let java = java_executable(Some(home));
    assert!(java.ends_with(if cfg!(windows) {
        "bin/java.exe"
    } else {
        "bin/java"
    }));
    assert!(java.starts_with(home));
}

#[test]
fn expected_class_output_uses_source_stem() {
    let (class_name, class_file) =
        expected_class_output(Path::new("corpus/Found_0042.java"), Path::new("out"));
    assert_eq!(class_name, "Found_0042");
    assert_eq!(class_file, Path::new("out").join("Found_0042.class"));
}

#[test]
fn subprocess_captures_stdout_and_exit_code() {
    // A trivial, always-present child: the host's own shell echo. This
    // exercises the capture/timeout plumbing without needing a JVM.
    let cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "echo difftest-ok"]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "echo difftest-ok"]);
        c
    };
    let obs = run_subprocess(cmd, Duration::from_secs(30)).expect("spawn");
    assert_eq!(obs.stdout, "difftest-ok");
    assert_eq!(obs.exit_code, Some(0));
    assert!(!obs.timed_out);
}

#[test]
fn subprocess_times_out_and_flags() {
    // A child that sleeps far longer than the timeout must be killed and
    // flagged, not block the test.
    // Spawn the long sleeper as a DIRECT child (no shell wrapper) so
    // killing it closes our captured pipes immediately — mirroring how a
    // real cratonvm/java timeout is handled. A `cmd /C ping` grandchild
    // would keep the inherited pipe open until ping exits and stall the
    // drain thread.
    let cmd = if cfg!(windows) {
        // `ping -n 30 127.0.0.1` sleeps ~29s without extra deps.
        let mut c = Command::new("ping");
        c.args(["-n", "30", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("30");
        c
    };
    let obs = run_subprocess(cmd, Duration::from_millis(300)).expect("spawn");
    assert!(obs.timed_out, "expected timeout flag");
}
