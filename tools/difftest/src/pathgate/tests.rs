use super::*;

fn split(mode: Mode, flaky: bool) -> Split {
    Split {
        mode,
        details: vec!["stdout: 1 != 2".into()],
        flaky,
    }
}

#[test]
fn path_gate_default_is_every_execution_path_plus_moving_gc() {
    let parsed = runner::parse_modes(DEFAULT_MODES).unwrap();
    for m in Mode::execution_paths() {
        assert!(parsed.contains(m), "{} missing from the default", m.label());
    }
    assert!(parsed.contains(&Mode::MovingGc));
    assert_eq!(parsed[0], Mode::NoJit, "the reference leads");
}

#[test]
fn a_new_reproducible_split_fails_the_gate() {
    let mut r = PathGateReport::default();
    let judged = [Mode::IrJit, Mode::OsrEager];
    r.record(
        "P",
        Mode::NoJit,
        &judged,
        vec![split(Mode::IrJit, false)],
        &[],
        &KnownSplits::default(),
    );
    assert_eq!(r.new_splits, vec!["P: nojit≠ir-jit".to_string()]);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn known_and_flaky_splits_do_not_fail_it() {
    let known = KnownSplits {
        schema_version: 1,
        entries: vec![KnownSplit {
            program: "P".into(),
            mode: "ir-jit".into(),
            note: String::new(),
        }],
    };
    let mut r = PathGateReport::default();
    r.record(
        "P",
        Mode::NoJit,
        &[Mode::IrJit, Mode::ForcedDeopt],
        vec![split(Mode::IrJit, false), split(Mode::ForcedDeopt, true)],
        &[],
        &known,
    );
    assert_eq!(r.known_splits.len(), 1);
    assert_eq!(r.flaky.len(), 1);
    assert!(r.new_splits.is_empty());
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn a_known_split_that_now_agrees_is_reported_resolved() {
    let known = KnownSplits {
        schema_version: 1,
        entries: vec![KnownSplit {
            program: "P".into(),
            mode: "osr-eager".into(),
            note: String::new(),
        }],
    };
    let mut r = PathGateReport::default();
    r.record("P", Mode::NoJit, &[Mode::OsrEager], vec![], &[], &known);
    assert_eq!(r.resolved_known, vec!["P: nojit≠osr-eager".to_string()]);
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn verifier_rejections_gate_only_when_asked() {
    let mut r = PathGateReport::default();
    r.record(
        "P",
        Mode::NoJit,
        &[Mode::IrJit],
        vec![],
        &[Mode::IrJit],
        &KnownSplits::default(),
    );
    assert_eq!(r.exit_code(), 0);
    r.fail_on_ir_verify_reject = true;
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn split_details_names_the_dimension() {
    let mut a = Observation::empty();
    a.exit_code = Some(0);
    a.stdout = "42".into();
    let mut b = a.clone();
    assert!(split_details(&a, &b, Mode::IrJit).is_none());
    b.stdout = "43".into();
    let d = split_details(&b, &a, Mode::IrJit).unwrap();
    assert!(d[0].starts_with("stdout: 43 != 42"), "{d:?}");
}

#[test]
fn the_committed_known_file_parses() {
    let k = KnownSplits::load(&default_known_path()).expect("path-gate-known.json parses");
    assert_eq!(k.schema_version, KNOWN_SCHEMA_VERSION);
    // A missing file is an empty list, not an error.
    let missing = KnownSplits::load(Path::new("does/not/exist.json")).unwrap();
    assert!(missing.entries.is_empty());
}
