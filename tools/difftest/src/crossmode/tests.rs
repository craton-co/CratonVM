use super::*;

fn obs(stdout: &str) -> Observation {
    Observation {
        stdout: stdout.to_string(),
        exit_code: Some(0),
        ..Observation::empty()
    }
}

#[test]
fn agreeing_paths_produce_no_split() {
    let a = obs("42");
    let b = obs("42");
    let c = obs("42");
    let runs = [(Mode::NoJit, &a), (Mode::DirectEmit, &b), (Mode::IrJit, &c)];
    assert!(disagreements(&runs).is_empty());
}

#[test]
fn a_path_split_is_reported_without_any_reference() {
    // The interpreter and the IR pipeline printed different things. That is
    // a wrong-code finding on its own — no HotSpot observation involved.
    let interp = obs("42");
    let ir = obs("43");
    let runs = [(Mode::NoJit, &interp), (Mode::IrJit, &ir)];
    let splits = disagreements(&runs);
    assert_eq!(splits.len(), 1);
    assert_eq!(splits[0].a, Mode::NoJit);
    assert_eq!(splits[0].b, Mode::IrJit);
    assert_eq!(splits[0].channels(), vec![Channel::Stdout]);
    assert_eq!(splits[0].label(), "nojit≠ir-jit[stdout]");
    assert!(render("P", &splits).contains("PATH  P — nojit≠ir-jit[stdout]"));
}

#[test]
fn every_disagreeing_pair_is_reported_not_just_the_first() {
    // Three paths, one odd one out ⇒ two splits, so a report cannot leave
    // the reader guessing which path is the outlier.
    let a = obs("42");
    let b = obs("42");
    let odd = obs("99");
    let runs = [
        (Mode::NoJit, &a),
        (Mode::DirectEmit, &b),
        (Mode::IrJit, &odd),
    ];
    let splits = disagreements(&runs);
    assert_eq!(splits.len(), 2);
    let labels: Vec<String> = splits.iter().map(|s| s.label()).collect();
    assert!(labels.contains(&"nojit≠ir-jit[stdout]".to_string()));
    assert!(labels.contains(&"direct-emit≠ir-jit[stdout]".to_string()));
}

#[test]
fn a_policy_pair_is_never_compared_as_a_path_split() {
    // `--real-jdk` refusing a compatibility class is a policy finding with
    // its own ledger row, not a semantic disagreement between executors.
    assert!(!comparable(Mode::JitOn, Mode::JdkOnlyJit));
    assert!(!comparable(Mode::NoJit, Mode::JdkOnlyNoJit));
    // Within one profile the pairs are comparable.
    assert!(comparable(Mode::NoJit, Mode::IrJit));
    assert!(comparable(Mode::JdkOnlyJit, Mode::JdkOnlyNoJit));
    // A mode is never compared with itself.
    assert!(!comparable(Mode::NoJit, Mode::NoJit));

    let strict = obs("refused");
    let compat = obs("ran");
    let runs = [(Mode::JdkOnlyJit, &strict), (Mode::JitOn, &compat)];
    assert!(
        disagreements(&runs).is_empty(),
        "a cross-policy pair must not be reported as a path split"
    );
}

#[test]
fn a_timeout_on_one_path_only_is_a_split() {
    // The `Hang` shape, seen without a reference: one executor finished and
    // another did not.
    let ok = obs("done");
    let mut hung = obs("");
    hung.timed_out = true;
    hung.exit_code = None;
    let runs = [(Mode::NoJit, &ok), (Mode::OsrEager, &hung)];
    let splits = disagreements(&runs);
    assert_eq!(splits.len(), 1);
    assert!(splits[0].channels().contains(&Channel::ExitCode));
}

#[test]
fn one_line_truncates_long_blobs() {
    assert_eq!(one_line("a\nb"), "a / b");
    let long = "x".repeat(200);
    let flat = one_line(&long);
    assert_eq!(flat.chars().count(), 80);
    assert!(flat.ends_with("..."));
}
