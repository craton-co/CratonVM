use super::*;
use crate::oracle::ChannelDiff;

use crate::ledger::{ViolationTally, PROFILE_COMPATIBLE, PROFILE_JDK_ONLY};

// A completed run has a real exit code; `exit_code: None` would (correctly)
// be read as a signal-kill ⇒ Crash, so these fixtures set `Some(0)`.
fn diverging_outcome(mode: Mode) -> ModeOutcome {
    ModeOutcome {
        mode,
        cratonvm: Observation {
            stdout: "42".into(),
            exit_code: Some(0),
            ..Observation::empty()
        },
        verdict: Verdict::Diverge(vec![ChannelDiff {
            channel: Channel::Stdout,
            cratonvm: "42".into(),
            hotspot: "43".into(),
        }]),
        census: None,
    }
}

fn agreeing_outcome(mode: Mode) -> ModeOutcome {
    ModeOutcome {
        mode,
        cratonvm: Observation {
            exit_code: Some(0),
            ..Observation::empty()
        },
        verdict: Verdict::Agree,
        census: None,
    }
}

/// A census carrying one violation of `kind`.
fn census_with(profile: &str, kind: &str) -> StrictCensus {
    let mut c = StrictCensus::profile_only(profile, Some(25));
    c.violations.push(ViolationTally {
        kind: kind.into(),
        count: 1,
        sample: Some("java/foo/Bar.baz()V".into()),
    });
    c
}

fn temp_program_dir(test_name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "cratonvm_difftest_{test_name}_{}_{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("create temp program dir");
    dir
}

#[test]
fn discover_finds_java_and_class_sorted() {
    let dir = temp_program_dir("discover");
    std::fs::write(dir.join("B.class"), b"class bytes").expect("write class");
    std::fs::write(dir.join("A.java"), "public class A {}\n").expect("write java");
    std::fs::write(dir.join("ignore.txt"), "not runnable\n").expect("write txt");

    let progs = discover_programs(&dir);
    assert!(progs.iter().all(|p| {
        matches!(
            p.extension().and_then(|s| s.to_str()),
            Some("java") | Some("class")
        )
    }));
    let mut sorted = progs.clone();
    sorted.sort();
    assert_eq!(progs, sorted);
    assert_eq!(progs.len(), 2);
    let names: Vec<_> = progs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["A.java".to_string(), "B.class".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discover_missing_dir_is_empty() {
    assert!(discover_programs(Path::new("does/not/exist")).is_empty());
}

#[test]
fn program_classifies_jit_only() {
    // jit-on diverges, nojit agrees ⇒ JitOnly + representative is jit-on.
    let r = ProgramResult {
        program: "J".into(),
        source: PathBuf::from("seeds/J.java"),
        hotspot: Observation::empty(),
        modes: vec![
            diverging_outcome(Mode::JitOn),
            agreeing_outcome(Mode::NoJit),
        ],
        jdk_feature: Some(25),
    };
    assert!(r.diverged());
    assert_eq!(r.classification(), Some(Classification::JitOnly));
    assert_eq!(r.representative().unwrap().mode, Mode::JitOn);
}

#[test]
fn summary_counts_classifies_and_ledger() {
    let agree = ProgramResult {
        program: "A".into(),
        source: PathBuf::from("seeds/A.java"),
        hotspot: Observation::empty(),
        modes: vec![agreeing_outcome(Mode::JitOn), agreeing_outcome(Mode::NoJit)],
        jdk_feature: Some(25),
    };
    // Both modes diverge ⇒ Universal (the ExceptionId shape).
    let universal = ProgramResult {
        program: "B".into(),
        source: PathBuf::from("seeds/B.java"),
        hotspot: Observation::empty(),
        modes: vec![
            diverging_outcome(Mode::JitOn),
            diverging_outcome(Mode::NoJit),
        ],
        jdk_feature: Some(25),
    };
    let summary = RunSummary {
        results: vec![agree, universal],
        ..Default::default()
    };
    assert_eq!(summary.total(), 2);
    assert_eq!(summary.diverged(), 1);

    let ledger = summary.to_ledger("host".into(), "t".into(), "25".into());
    assert_eq!(ledger.entries.len(), 1);
    assert_eq!(ledger.entries[0].class, "B");
    assert_eq!(ledger.entries[0].classification, Classification::Universal);
    assert_eq!(ledger.entries[0].status, LedgerStatus::New);
    // A legacy-mode run writes plain compatible rows, as it always has.
    assert_eq!(ledger.entries[0].jdk_profile, PROFILE_COMPATIBLE);
    assert_eq!(ledger.entries[0].jdk_feature, Some(25));
    assert!(ledger.entries[0].jdk_only_violations.is_empty());
    assert_eq!(ledger.schema_version, crate::ledger::LEDGER_SCHEMA_VERSION);

    let rendered = render_summary(&summary);
    assert!(rendered.contains("OK    A"));
    assert!(rendered.contains("DIFF  B [universal]"));
    assert!(rendered.contains("jit-on:stdout"));
    assert!(rendered.contains("nojit:stdout"));
}

// -- gate (design §3.5) -------------------------------------------------

/// A program that diverges in both modes (the Universal shape).
fn diverging_program(name: &str) -> ProgramResult {
    program_with(
        name,
        vec![
            diverging_outcome(Mode::JitOn),
            diverging_outcome(Mode::NoJit),
        ],
    )
}

fn agreeing_program(name: &str) -> ProgramResult {
    program_with(
        name,
        vec![agreeing_outcome(Mode::JitOn), agreeing_outcome(Mode::NoJit)],
    )
}

fn program_with(name: &str, modes: Vec<ModeOutcome>) -> ProgramResult {
    ProgramResult {
        program: name.into(),
        source: PathBuf::from(format!("seeds/{name}.java")),
        hotspot: Observation::empty(),
        modes,
        jdk_feature: Some(25),
    }
}

/// A ledger holding one `compatible` entry for `class`.
fn ledger_with(class: &str, status: LedgerStatus, cratonvm_stdout: &str) -> Ledger {
    ledger_row(class, PROFILE_COMPATIBLE, status, cratonvm_stdout)
}

/// A ledger holding one entry for `(class, profile)` with the given status;
/// its recorded CratonVM stdout is `cratonvm_stdout`.
fn ledger_row(
    class: &str,
    profile: &str,
    status: LedgerStatus,
    cratonvm_stdout: &str,
) -> Ledger {
    let mut l = Ledger::new("host".into(), "t".into(), "25".into());
    l.entries.push(LedgerEntry {
        id: "div-0000".into(),
        class: class.into(),
        repro_path: format!("seeds/{class}.java"),
        classification: Classification::Universal,
        cratonvm: Observation {
            stdout: cratonvm_stdout.into(),
            exit_code: Some(0),
            ..Observation::empty()
        },
        hotspot: Observation::empty(),
        status,
        first_seen: "t".into(),
        linked_doc: None,
        jdk_profile: profile.into(),
        jdk_feature: Some(25),
        class_origins: Default::default(),
        native_invocations: Default::default(),
        jdk_only_violations: Vec::new(),
    });
    l
}

#[test]
fn update_ledger_preserves_existing_triage_and_links() {
    let mut existing = ledger_with("X", LedgerStatus::Known, "old");
    existing.host = "old-host".into();
    existing.captured_at = "old-capture".into();
    existing.jdk = "old-jdk".into();
    existing.entries[0].first_seen = "old-first-seen".into();
    existing.entries[0].linked_doc = Some("x.md".into());
    existing.entries.push(LedgerEntry {
        id: "div-0001".into(),
        class: "Z".into(),
        repro_path: "seeds/Z.java".into(),
        classification: Classification::Universal,
        cratonvm: Observation {
            stdout: "stale".into(),
            exit_code: Some(0),
            ..Observation::empty()
        },
        hotspot: Observation::empty(),
        status: LedgerStatus::Fixed,
        first_seen: "fixed-first-seen".into(),
        linked_doc: Some("z.md".into()),
        jdk_profile: PROFILE_COMPATIBLE.into(),
        jdk_feature: Some(25),
        class_origins: Default::default(),
        native_invocations: Default::default(),
        jdk_only_violations: Vec::new(),
    });

    let summary = summary_of(vec![diverging_program("X"), diverging_program("Y")]);
    let merged = summary.to_merged_ledger(
        Some(&existing),
        "new-host".into(),
        "new-capture".into(),
        "new-jdk".into(),
    );

    assert_eq!(merged.host, "new-host");
    assert_eq!(merged.captured_at, "new-capture");
    assert_eq!(merged.jdk, "new-jdk");
    assert_eq!(merged.entries.len(), 3);

    let x = merged.entries.iter().find(|e| e.class == "X").unwrap();
    assert_eq!(x.id, "div-0000");
    assert_eq!(x.status, LedgerStatus::Known);
    assert_eq!(x.first_seen, "old-first-seen");
    assert_eq!(x.linked_doc.as_deref(), Some("x.md"));
    assert_eq!(x.cratonvm.stdout, "42");

    let z = merged.entries.iter().find(|e| e.class == "Z").unwrap();
    assert_eq!(z.status, LedgerStatus::Fixed);
    assert_eq!(z.linked_doc.as_deref(), Some("z.md"));

    let y = merged.entries.iter().find(|e| e.class == "Y").unwrap();
    assert_eq!(y.id, "div-0002");
    assert_eq!(y.status, LedgerStatus::New);
    assert_eq!(y.first_seen, "new-capture");
    assert!(y.linked_doc.is_none());
}

fn summary_of(results: Vec<ProgramResult>) -> RunSummary {
    RunSummary {
        results,
        ..Default::default()
    }
}

#[test]
fn gate_new_divergence_fails_exit_1() {
    // Diverges, but nothing in the (empty) ledger ⇒ new.
    let summary = summary_of(vec![diverging_program("X")]);
    let ledger = Ledger::new("h".into(), "t".into(), "25".into());
    let report = gate(&summary, &ledger);
    assert_eq!(report.new, vec!["X".to_string()]);
    assert_eq!(report.exit_code(), 1);
    assert!(!report.is_clean());
}

#[test]
fn gate_known_divergence_is_allowed_exit_0() {
    // The diverging_outcome's recorded stdout is "42"; a Known entry with
    // the same stdout matches ⇒ allowed.
    let summary = summary_of(vec![diverging_program("X")]);
    let ledger = ledger_with("X", LedgerStatus::Known, "42");
    let report = gate(&summary, &ledger);
    assert_eq!(report.known, vec!["X".to_string()]);
    assert!(report.new.is_empty());
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn gate_drift_fails_exit_1() {
    // Known entry recorded a different CratonVM stdout ⇒ drift.
    let summary = summary_of(vec![diverging_program("X")]);
    let ledger = ledger_with("X", LedgerStatus::Known, "99-was-different");
    let report = gate(&summary, &ledger);
    assert_eq!(report.drifted, vec!["X".to_string()]);
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn gate_fixed_redivergence_fails_exit_2() {
    let summary = summary_of(vec![diverging_program("X")]);
    let ledger = ledger_with("X", LedgerStatus::Fixed, "42");
    let report = gate(&summary, &ledger);
    assert_eq!(report.regressed, vec!["X".to_string()]);
    assert_eq!(report.exit_code(), 2);
}

#[test]
fn gate_resolved_is_clean() {
    // A Known divergence that now agrees ⇒ resolved (a fix), exit 0.
    let summary = summary_of(vec![agreeing_program("X")]);
    let ledger = ledger_with("X", LedgerStatus::Known, "42");
    let report = gate(&summary, &ledger);
    assert_eq!(report.resolved, vec!["X".to_string()]);
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn gate_all_agree_empty_ledger_is_clean() {
    let summary = summary_of(vec![agreeing_program("A"), agreeing_program("B")]);
    let ledger = Ledger::new("h".into(), "t".into(), "25".into());
    let report = gate(&summary, &ledger);
    assert!(report.is_clean());
    assert_eq!(report.exit_code(), 0);
}

// -- (class, jdk_profile) keying ----------------------------------------

/// A program run under the two strict modes, diverging in both.
fn strict_diverging_program(name: &str) -> ProgramResult {
    program_with(
        name,
        vec![
            diverging_outcome(Mode::JdkOnlyJit),
            diverging_outcome(Mode::JdkOnlyNoJit),
        ],
    )
}

#[test]
fn a_known_compatible_row_does_not_excuse_a_strict_divergence() {
    // The whole reason the profile is half the key. Before this, a `known`
    // compatible entry for `X` would be found by class name and the strict
    // divergence would gate as "known (allowed)" — the exact silent pass
    // this feature exists to prevent.
    let summary = summary_of(vec![strict_diverging_program("X")]);
    let ledger = ledger_with("X", LedgerStatus::Known, "42");
    let report = gate(&summary, &ledger);
    assert_eq!(report.new, vec!["X@jdk-only".to_string()]);
    assert!(report.known.is_empty(), "the compatible row must not match");
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn a_strict_row_matches_only_its_own_profile() {
    let summary = summary_of(vec![strict_diverging_program("X")]);
    let ledger = ledger_row("X", PROFILE_JDK_ONLY, LedgerStatus::Known, "42");
    let report = gate(&summary, &ledger);
    assert_eq!(report.known, vec!["X@jdk-only".to_string()]);
    assert!(report.new.is_empty());
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn one_program_under_both_policies_produces_two_rows() {
    // `--modes nojit,jdk-only-jit`: the compatible row is known, the strict
    // one is new. Two findings, judged independently.
    let r = program_with(
        "X",
        vec![
            diverging_outcome(Mode::NoJit),
            diverging_outcome(Mode::JdkOnlyJit),
        ],
    );
    assert_eq!(r.profiles(), vec![PROFILE_COMPATIBLE, PROFILE_JDK_ONLY]);
    assert!(r.diverged_in(PROFILE_COMPATIBLE) && r.diverged_in(PROFILE_JDK_ONLY));

    let summary = summary_of(vec![r]);
    assert_eq!(summary.diverged(), 1, "one program");
    assert_eq!(summary.diverged_rows(), 2, "two ledger rows");

    let report = gate(&summary, &ledger_with("X", LedgerStatus::Known, "42"));
    assert_eq!(report.known, vec!["X".to_string()]);
    assert_eq!(report.new, vec!["X@jdk-only".to_string()]);
    assert_eq!(report.exit_code(), 1);

    let ledger = summary.to_ledger("h".into(), "t".into(), "25".into());
    assert_eq!(ledger.entries.len(), 2);
    let profiles: Vec<&str> = ledger
        .entries
        .iter()
        .map(|e| e.jdk_profile.as_str())
        .collect();
    assert_eq!(profiles, vec![PROFILE_COMPATIBLE, PROFILE_JDK_ONLY]);
    // Ids stay unique across rows of the same class.
    assert_ne!(ledger.entries[0].id, ledger.entries[1].id);
}

#[test]
fn merged_ledger_updates_each_profile_row_independently() {
    let mut existing = ledger_row("X", PROFILE_JDK_ONLY, LedgerStatus::Known, "old");
    existing.entries[0].linked_doc = Some("x.md".into());
    existing.entries[0].first_seen = "old-first-seen".into();

    let summary = summary_of(vec![program_with(
        "X",
        vec![
            diverging_outcome(Mode::NoJit),
            diverging_outcome(Mode::JdkOnlyJit),
        ],
    )]);
    let merged =
        summary.to_merged_ledger(Some(&existing), "h".into(), "cap".into(), "25".into());

    assert_eq!(merged.entries.len(), 2);
    let strict = merged.find("X", PROFILE_JDK_ONLY).expect("strict row");
    // The pre-existing strict row keeps its triage and gets fresh data.
    assert_eq!(strict.status, LedgerStatus::Known);
    assert_eq!(strict.first_seen, "old-first-seen");
    assert_eq!(strict.linked_doc.as_deref(), Some("x.md"));
    assert_eq!(strict.cratonvm.stdout, "42");
    // The compatible row is brand new and did not inherit the strict row's
    // triage.
    let compat = merged
        .find("X", PROFILE_COMPATIBLE)
        .expect("compatible row");
    assert_eq!(compat.status, LedgerStatus::New);
    assert_eq!(compat.first_seen, "cap");
    assert!(compat.linked_doc.is_none());
}

#[test]
fn legacy_only_runs_key_exactly_as_before() {
    // A run using only the historical modes must be judged against the
    // migrated schema-1 rows with no change in verdict.
    let mut ledger = ledger_with("X", LedgerStatus::Known, "42");
    ledger.schema_version = 1;
    ledger.migrate();
    let summary = summary_of(vec![diverging_program("X")]);
    let report = gate(&summary, &ledger);
    assert_eq!(report.known, vec!["X".to_string()], "bare class label");
    assert_eq!(report.exit_code(), 0);
}

// -- strict violations ---------------------------------------------------

#[test]
fn strict_violations_are_reported_even_when_the_run_agreed() {
    let mut outcome = agreeing_outcome(Mode::JdkOnlyJit);
    outcome.census = Some(census_with(PROFILE_JDK_ONLY, "missing-native"));
    let summary = summary_of(vec![program_with("X", vec![outcome])]);
    let report = gate(&summary, &Ledger::new("h".into(), "t".into(), "25".into()));

    assert_eq!(report.strict_violations.len(), 1);
    let v = &report.strict_violations[0];
    assert_eq!(v.program, "X");
    assert_eq!(v.mode, "jdk-only-jit");
    assert_eq!(v.jdk_profile, PROFILE_JDK_ONLY);
    assert_eq!(v.kind, "missing-native");
    assert_eq!(report.violation_count(), 1);

    // Measurement, not enforcement (contract §10): the exit code is
    // untouched and the gate still passes.
    assert_eq!(report.exit_code(), 0);
    assert!(report.is_clean());
    assert!(report.new.is_empty() && report.drifted.is_empty());
    assert!(render_gate(&report).contains("measured, not gated"));
}

#[test]
fn a_violation_that_changed_behaviour_gates_as_the_divergence_it_is() {
    let mut outcome = diverging_outcome(Mode::JdkOnlyJit);
    outcome.census = Some(census_with(
        PROFILE_JDK_ONLY,
        "compatibility-class-requested",
    ));
    let summary = summary_of(vec![program_with("X", vec![outcome])]);
    let report = gate(&summary, &Ledger::new("h".into(), "t".into(), "25".into()));
    // It gates once, as a new divergence — not twice.
    assert_eq!(report.new, vec!["X@jdk-only".to_string()]);
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.strict_violations.len(), 1);
}

#[test]
fn compatible_mode_violations_are_measured_but_not_a_violation_verdict() {
    // `real-compatible-*` records what a strict run *would* have refused.
    // It must be reported, but it must not classify the run as violating.
    let mut outcome = agreeing_outcome(Mode::RealCompatibleJit);
    outcome.census = Some(census_with(
        PROFILE_COMPATIBLE,
        "compatibility-class-requested",
    ));
    let r = program_with("X", vec![outcome]);
    assert_eq!(
        r.classification(),
        None,
        "a would-be refusal is not a finding"
    );

    let report = gate(
        &summary_of(vec![r]),
        &Ledger::new("h".into(), "t".into(), "25".into()),
    );
    assert_eq!(report.strict_violations.len(), 1, "still measured");
    assert_eq!(report.strict_violations[0].jdk_profile, PROFILE_COMPATIBLE);
    assert!(report.is_clean());
}

#[test]
fn a_strict_violation_classifies_and_lands_on_the_ledger_row() {
    let mut outcome = diverging_outcome(Mode::JdkOnlyJit);
    let mut census = census_with(PROFILE_JDK_ONLY, "missing-native");
    census.class_origins.insert("boot-image".into(), 312);
    census.native_invocations.insert("synthetic-stub".into(), 0);
    outcome.census = Some(census);
    let summary = summary_of(vec![program_with("X", vec![outcome])]);

    let ledger = summary.to_ledger("h".into(), "t".into(), "25".into());
    let e = ledger.find("X", PROFILE_JDK_ONLY).expect("strict row");
    assert_eq!(e.classification, Classification::JdkOnlyViolation);
    assert_eq!(e.jdk_feature, Some(25));
    assert_eq!(e.class_origins["boot-image"], 312);
    assert_eq!(e.native_invocations["synthetic-stub"], 0);
    assert_eq!(e.jdk_only_violations.len(), 1);
    assert_eq!(e.jdk_only_violations[0].kind, "missing-native");
}

// -- guard_profile -------------------------------------------------------

#[test]
fn guard_profile_flags_a_silent_fallback() {
    // A `--real-jdk` child that reports `compatible` fell back; the whole
    // run described a policy nobody asked for.
    let mut census = StrictCensus::profile_only(PROFILE_COMPATIBLE, Some(25));
    assert!(guard_profile(Mode::JdkOnlyJit, &mut census));
    assert!(census.has_violations());
    assert_eq!(census.violations[0].kind, "profile-mismatch");
    assert!(census.violations[0]
        .sample
        .as_deref()
        .unwrap()
        .contains("compatible"));
    // The census keeps what the run *reported*; the row is keyed by what
    // was requested.
    assert_eq!(census.jdk_profile, PROFILE_COMPATIBLE);
}

#[test]
fn guard_profile_is_silent_when_the_profile_matches() {
    let mut census = StrictCensus::profile_only(PROFILE_JDK_ONLY, Some(25));
    assert!(!guard_profile(Mode::JdkOnlyJit, &mut census));
    assert!(!census.has_violations());
}

#[test]
fn guard_profile_ignores_modes_that_did_not_ask_for_strictness() {
    // `real-compatible-*` and the historical five report `compatible`
    // because that is what they asked for.
    for mode in [Mode::RealCompatibleJit, Mode::JitOn, Mode::NoJit] {
        let mut census = StrictCensus::profile_only(PROFILE_COMPATIBLE, Some(25));
        assert!(!guard_profile(mode, &mut census), "{}", mode.label());
        assert!(!census.has_violations());
    }
}

#[test]
fn a_fallback_makes_the_run_a_violation_verdict() {
    // End to end: the mismatch guard is what turns an otherwise-clean
    // strict run into a reported finding.
    let mut outcome = agreeing_outcome(Mode::JdkOnlyJit);
    let mut census = StrictCensus::profile_only(PROFILE_COMPATIBLE, Some(25));
    guard_profile(Mode::JdkOnlyJit, &mut census);
    outcome.census = Some(census);
    let r = program_with("X", vec![outcome]);
    assert_eq!(
        r.classification_in(PROFILE_JDK_ONLY),
        Some(Classification::JdkOnlyViolation)
    );
    let rendered = render_summary(&summary_of(vec![r]));
    assert!(
        rendered.contains("jdk-only violation(s) recorded"),
        "{rendered}"
    );
}

// -- census-free modes are untouched -------------------------------------

// -- cross-path (reference-free) findings --------------------------------

#[test]
fn two_execution_paths_disagreeing_is_reported_but_does_not_gate() {
    // `nojit` printed "42" and `ir-jit` printed nothing: one of the VM's own
    // executors is wrong, and no reference JDK is needed to know it. It is
    // surfaced on the gate report and deliberately left out of the exit
    // code — a `JitOnly` row already on the ledger is a path split by
    // construction, so gating here would fail the frozen baseline.
    let r = program_with(
        "X",
        vec![
            diverging_outcome(Mode::NoJit),
            agreeing_outcome(Mode::IrJit),
        ],
    );
    let splits = r.path_disagreements();
    assert_eq!(splits.len(), 1);
    assert_eq!(splits[0].label(), "nojit≠ir-jit[stdout]");

    // The ledger says this program is a known divergence, so the gate is
    // clean — and the split is still reported.
    let report = gate(
        &summary_of(vec![r]),
        &ledger_with("X", LedgerStatus::Known, "42"),
    );
    assert_eq!(
        report.path_splits,
        vec!["X: nojit≠ir-jit[stdout]".to_string()]
    );
    assert_eq!(report.exit_code(), 0, "a path split must not move the gate");
    assert!(report.is_clean());
    assert!(render_gate(&report).contains("reported, not gated"));
}

#[test]
fn paths_that_agree_produce_no_split_even_when_all_diverge_from_hotspot() {
    // The `Universal` shape: every executor is wrong in the same way. There
    // is no path split, because the paths agree with each other — which is
    // itself the useful signal (the bug is shared, not JIT-specific).
    let r = diverging_program("X");
    assert!(r.path_disagreements().is_empty());
    let report = gate(
        &summary_of(vec![r]),
        &Ledger::new("h".into(), "t".into(), "25".into()),
    );
    assert!(report.path_splits.is_empty());
    assert!(!render_gate(&report).contains("reported, not gated"));
}

#[test]
fn a_cross_policy_pair_is_never_reported_as_a_path_split() {
    // `--real-jdk` refusing something the compatible run allowed is a policy
    // finding with its own ledger row, not two executors disagreeing.
    let r = program_with(
        "X",
        vec![
            diverging_outcome(Mode::JitOn),
            agreeing_outcome(Mode::JdkOnlyJit),
        ],
    );
    assert!(
        r.path_disagreements().is_empty(),
        "the policy axis must not masquerade as an execution-path split"
    );
}

#[test]
fn the_summary_names_the_dimension_that_moved() {
    // The whole point of splitting the exception channel: a report says
    // `exception-message`, not `exception`.
    let mut outcome = agreeing_outcome(Mode::JitOn);
    outcome.verdict = Verdict::Diverge(vec![ChannelDiff {
        channel: Channel::ExceptionMessage,
        cratonvm: "no detail".into(),
        hotspot: "Index 9 out of bounds for length 3".into(),
    }]);
    let rendered = render_summary(&summary_of(vec![program_with("X", vec![outcome])]));
    assert!(rendered.contains("jit-on:exception-message"), "{rendered}");
}

#[test]
fn legacy_modes_carry_no_census_and_no_violation() {
    let r = diverging_program("X");
    for m in &r.modes {
        assert!(
            m.census.is_none(),
            "{} must not collect a census",
            m.mode.label()
        );
    }
    assert_eq!(r.profiles(), vec![PROFILE_COMPATIBLE]);
    assert_eq!(r.classification(), Some(Classification::Universal));
    let report = gate(
        &summary_of(vec![r]),
        &Ledger::new("h".into(), "t".into(), "25".into()),
    );
    assert!(report.strict_violations.is_empty());
    assert_eq!(report.new, vec!["X".to_string()]);
}
