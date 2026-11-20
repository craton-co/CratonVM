use super::*;

fn obs(stdout: &str, stderr: &str, exit: Option<i32>) -> Observation {
    Observation {
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
        exit_code: exit,
        exception: parse_exception(stderr),
        timed_out: false,
        wall_ms: 0,
    }
}

/// The channels a verdict reported, in report order — the shape every
/// "planted divergence" test below asserts on.
fn channels(v: &Verdict) -> Vec<Channel> {
    match v {
        Verdict::Agree => Vec::new(),
        Verdict::Diverge(d) => d.iter().map(|c| c.channel).collect(),
    }
}

#[test]
fn line_endings_normalized() {
    assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb");
    assert_eq!(normalize_line_endings("a\nb\n\n  "), "a\nb");
}

#[test]
fn parses_exception_with_message_and_frames() {
    let stderr = "Exception in thread \"main\" java.lang.ArithmeticException: / by zero\n\
                  \tat ArithEdge.main(ArithEdge.java:5)\n";
    let ex = parse_exception(stderr).expect("parsed");
    assert_eq!(ex.fqcn, "java.lang.ArithmeticException");
    assert_eq!(ex.message, "/ by zero");
    assert_eq!(ex.top_frames, vec!["ArithEdge.main(ArithEdge.java:5)"]);
}

#[test]
fn parses_exception_without_message() {
    let stderr = "Exception in thread \"main\" java.lang.NullPointerException\n";
    let ex = parse_exception(stderr).expect("parsed");
    assert_eq!(ex.fqcn, "java.lang.NullPointerException");
    assert_eq!(ex.message, "");
}

#[test]
fn no_banner_is_none() {
    assert!(parse_exception("hello\nworld\n").is_none());
}

#[test]
fn identical_runs_agree() {
    let a = obs("42\n", "", Some(0));
    let b = obs("42\n", "", Some(0));
    assert!(compare(&a, &b, &Normalizer::strict()).agrees());
}

#[test]
fn stdout_divergence_is_caught() {
    let a = obs("42", "", Some(0));
    let b = obs("43", "", Some(0));
    let v = compare(&a, &b, &Normalizer::strict());
    match v {
        Verdict::Diverge(d) => {
            assert_eq!(d.len(), 1);
            assert_eq!(d[0].channel, Channel::Stdout);
        }
        Verdict::Agree => panic!("expected divergence"),
    }
}

// -- one planted divergence per dimension --------------------------------
//
// The harness's own soundness property: each dimension must *independently*
// detect a divergence planted in it alone, and must name itself when it
// does. A dimension that only ever fires together with `stdout` is not a
// dimension, it is a duplicate.

#[test]
fn exit_code_divergence_is_caught_alone() {
    let a = obs("same", "", Some(0));
    let b = obs("same", "", Some(1));
    assert_eq!(
        channels(&compare(&a, &b, &Normalizer::strict())),
        vec![Channel::ExitCode]
    );
}

#[test]
fn exception_presence_divergence_is_caught_alone() {
    // One VM threw, the other exited cleanly. Reported once, on the
    // presence channel — not three times against `<none>`.
    let a = obs(
        "",
        "Exception in thread \"main\" java.lang.IllegalStateException: boom\n",
        Some(1),
    );
    let b = obs("", "", Some(1));
    assert_eq!(
        channels(&compare(&a, &b, &Normalizer::strict())),
        vec![Channel::Exception]
    );
}

#[test]
fn exception_type_divergence_is_caught_alone() {
    let a = obs(
        "",
        "Exception in thread \"main\" java.lang.ClassCastException: x\n",
        Some(1),
    );
    let b = obs(
        "",
        "Exception in thread \"main\" java.lang.IllegalStateException: x\n",
        Some(1),
    );
    assert_eq!(
        channels(&compare(&a, &b, &Normalizer::strict())),
        vec![Channel::ExceptionType],
        "a wrong type must not be reported as a message or frame diff"
    );
}

#[test]
fn exception_message_divergence_is_caught_alone() {
    // The committed `ExceptionId` shape: right type, right frames, a
    // message missing HotSpot's module/loader detail.
    let a = obs(
        "",
        "Exception in thread \"main\" java.lang.ClassCastException: \
         java.lang.String cannot be cast to java.lang.Integer\n\tat X.main(X.java:3)\n",
        Some(1),
    );
    let b = obs(
        "",
        "Exception in thread \"main\" java.lang.ClassCastException: \
         class java.lang.String cannot be cast to class java.lang.Integer \
         (in module java.base)\n\tat X.main(X.java:3)\n",
        Some(1),
    );
    assert_eq!(
        channels(&compare(&a, &b, &Normalizer::strict())),
        vec![Channel::ExceptionMessage]
    );
}

#[test]
fn exception_frame_order_divergence_is_caught_alone() {
    // Same type, same message, frames in the wrong order — the reversed-
    // stack-trace bug this dimension exists for.
    let a = obs(
        "",
        "Exception in thread \"main\" java.lang.IllegalStateException: x\n\
         \tat A.a(A.java:1)\n\tat B.b(B.java:2)\n",
        Some(1),
    );
    let b = obs(
        "",
        "Exception in thread \"main\" java.lang.IllegalStateException: x\n\
         \tat B.b(B.java:2)\n\tat A.a(A.java:1)\n",
        Some(1),
    );
    assert_eq!(
        channels(&compare(&a, &b, &Normalizer::strict())),
        vec![Channel::ExceptionFrames]
    );
}

#[test]
fn checksum_divergence_is_caught_and_names_the_quantity() {
    let a = obs("##DIFFTEST-CHECKSUM## arith 111\n", "", Some(0));
    let b = obs("##DIFFTEST-CHECKSUM## arith 222\n", "", Some(0));
    match compare(&a, &b, &Normalizer::strict()) {
        Verdict::Diverge(d) => {
            // stdout moved too (the declaration is on stdout), but the
            // checksum dimension is what names the quantity.
            let sum = d
                .iter()
                .find(|c| c.channel == Channel::Checksum)
                .expect("checksum channel");
            assert_eq!(sum.cratonvm, "arith=111");
            assert_eq!(sum.hotspot, "arith=222");
        }
        Verdict::Agree => panic!("expected a checksum divergence"),
    }
}

#[test]
fn a_checksum_survives_an_over_normalizing_stdout_rule() {
    // The false-negative guard, and the reason the checksum is read from
    // un-normalized stdout. Two runs computed different digests; the seed
    // prints them in `0x…` form, so `mask_hashes` rewrites both to
    // `0x<addr>` and the stdout dimension goes quiet. The checksum
    // dimension does not — and it is the only one that fires, so the report
    // says exactly what happened.
    let a = obs("##DIFFTEST-CHECKSUM## total 0xdeadbeef\n", "", Some(0));
    let b = obs("##DIFFTEST-CHECKSUM## total 0xcafebabe\n", "", Some(0));
    let n = Normalizer {
        mask_hashes: true,
        ..Normalizer::strict()
    };
    assert!(
        n.apply(&a.stdout) == n.apply(&b.stdout),
        "the rule must genuinely have hidden the stdout diff"
    );
    assert_eq!(
        channels(&compare(&a, &b, &n)),
        vec![Channel::Checksum],
        "an over-normalizing rule hid the stdout diff; the checksum must not be hidden too"
    );
}

#[test]
fn matching_checksums_are_not_a_dimension() {
    // Neither declaring, and both declaring the same value, must be silent
    // — a vacuous pass is not evidence.
    let a = obs("plain\n", "", Some(0));
    let b = obs("plain\n", "", Some(0));
    assert!(compare(&a, &b, &Normalizer::strict()).agrees());
    let c = obs("##DIFFTEST-CHECKSUM## k v\n", "", Some(0));
    let d = obs("##DIFFTEST-CHECKSUM## k v\n", "", Some(0));
    assert!(compare(&c, &d, &Normalizer::strict()).agrees());
}

#[test]
fn identical_observations_report_no_divergence_on_any_dimension() {
    // The other half of every dimension test: a same-input/same-output pair
    // must be silent on all eight channels, under every built-in profile.
    let stderr = "Exception in thread \"main\" java.lang.IllegalStateException: x\n\
                  \tat A.a(A.java:1)\n";
    let a = obs("out\n##DIFFTEST-CHECKSUM## k 1\n", stderr, Some(1));
    let b = a.clone();
    for n in [Normalizer::strict(), Normalizer::jdk_only()] {
        assert!(
            compare(&a, &b, &n).agrees(),
            "identical observations diverged under {}",
            n.describe()
        );
    }
}

#[test]
fn timeout_never_matches_clean_exit() {
    let mut a = obs("", "", None);
    a.timed_out = true;
    let b = obs("", "", Some(0));
    let v = compare(&a, &b, &Normalizer::strict());
    match v {
        Verdict::Diverge(d) => assert!(d.iter().any(|c| c.channel == Channel::ExitCode)),
        Verdict::Agree => panic!("timeout must diverge from clean exit"),
    }
}

fn mv(mode: Mode, diverged: bool) -> ModeVerdict {
    ModeVerdict {
        mode,
        diverged,
        timed_out: false,
        crashed: false,
        jdk_only_violation: false,
    }
}

#[test]
fn classify_all_agree_is_none() {
    let modes = [mv(Mode::JitOn, false), mv(Mode::NoJit, false)];
    assert_eq!(classify(&modes), None);
}

#[test]
fn classify_jit_only() {
    // jit-on diverges, nojit agrees ⇒ JitOnly (the --nojit bisection).
    let modes = [mv(Mode::JitOn, true), mv(Mode::NoJit, false)];
    assert_eq!(classify(&modes), Some(Classification::JitOnly));
}

#[test]
fn classify_universal_when_both_diverge() {
    // The ExceptionId case: an interpreter/native gap shows in BOTH modes.
    let modes = [mv(Mode::JitOn, true), mv(Mode::NoJit, true)];
    assert_eq!(classify(&modes), Some(Classification::Universal));
}

#[test]
fn classify_gc_mode() {
    // jit-on + nojit agree, only the moving-GC mode diverges.
    let modes = [
        mv(Mode::JitOn, false),
        mv(Mode::NoJit, false),
        mv(Mode::MovingGc, true),
    ];
    assert_eq!(classify(&modes), Some(Classification::GcMode));
}

#[test]
fn classify_low_threshold_is_jit_only() {
    // A JIT bug that only trips when compilation is forced early.
    let modes = [
        mv(Mode::JitOn, false),
        mv(Mode::NoJit, false),
        mv(Mode::LowJitThreshold, true),
    ];
    assert_eq!(classify(&modes), Some(Classification::JitOnly));
}

#[test]
fn classify_hang_and_crash_dominate() {
    let mut hang = mv(Mode::JitOn, true);
    hang.timed_out = true;
    assert_eq!(classify(&[hang]), Some(Classification::Hang));
    let mut crash = mv(Mode::JitOn, true);
    crash.crashed = true;
    assert_eq!(classify(&[crash]), Some(Classification::Crash));
}

#[test]
fn first_diff_points_at_line() {
    let d = first_diff("a\nb\nc", "a\nX\nc");
    assert_eq!(d, Some((2, "b".to_string(), "X".to_string())));
    assert_eq!(first_diff("same", "same"), None);
}

// -- profile-mode normalization ------------------------------------------

/// A real captured CratonVM stderr: colourised `tracing` lines plus the
/// VM's own bracketed notices (lifted from the committed ledger's
/// `ExceptionId` row).
const VM_CHATTER: &str = concat!(
    "\u{1b}[2m2026-07-30T18:35:56.284528Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m ",
    "\u{1b}[2mcratonvm_vm::vm::vm_object\u{1b}[0m\u{1b}[2m:\u{1b}[0m ",
    "[NativeBridge] 1 unregistered native methods:\n",
    "[cratonvm] main-vm run() returned Ok — VM main exiting normally\n"
);

#[test]
fn strict_normalizer_is_byte_identical_to_step_1() {
    let n = Normalizer::strict();
    assert!(!n.gate_stderr);
    assert!(!n.strip_vm_diagnostics);
    assert_eq!(n, Normalizer::default());
    // stdout and stderr both get line-ending hygiene and nothing else — the
    // VM chatter is preserved verbatim, because a historical mode's
    // recorded stderr must not change shape.
    assert_eq!(n.apply("a\r\nb\r\n"), "a\nb");
    assert_eq!(
        n.apply_stderr(VM_CHATTER),
        normalize_line_endings(VM_CHATTER)
    );
    assert!(n.apply_stderr(VM_CHATTER).contains("\u{1b}["));
}

#[test]
fn strict_normalizer_does_not_gate_stderr() {
    // The historical behaviour: stderr differs wildly and is not judged.
    let a = obs("42", "[cratonvm] chatter", Some(0));
    let b = obs("42", "", Some(0));
    assert!(compare(&a, &b, &Normalizer::strict()).agrees());
}

#[test]
fn jdk_only_normalizer_strips_ansi_and_vm_lines() {
    let n = Normalizer::jdk_only();
    assert!(n.gate_stderr && n.strip_vm_diagnostics);
    assert_eq!(n.apply_stderr(VM_CHATTER), "");
    // A program's own stderr survives; only VM chatter goes.
    let mixed = format!("{VM_CHATTER}real program stderr\n");
    assert_eq!(n.apply_stderr(&mixed), "real program stderr");
}

#[test]
fn jdk_only_normalizer_leaves_stdout_byte_exact() {
    // stdout must never be laundered: a policy refusal that changed the
    // program's output is a plain divergence and has to be caught as one.
    let n = Normalizer::jdk_only();
    let noisy = "[cratonvm] this came from the program\ncratonvm_vm printed this";
    assert_eq!(n.apply(noisy), noisy);
}

#[test]
fn jdk_only_normalizer_gates_stderr_after_stripping() {
    let n = Normalizer::jdk_only();
    // Chatter-only stderr vs HotSpot's silence: agrees.
    let a = obs("42", VM_CHATTER, Some(0));
    let b = obs("42", "", Some(0));
    assert!(
        compare(&a, &b, &n).agrees(),
        "VM chatter alone is not a diff"
    );

    // A real refusal message on stderr is a Stderr-channel divergence.
    let c = obs("42", "jdk-only: refused java/foo/Bar", Some(0));
    match compare(&c, &b, &n) {
        Verdict::Diverge(d) => {
            assert_eq!(d.len(), 1);
            assert_eq!(d[0].channel, Channel::Stderr);
            assert_eq!(d[0].cratonvm, "jdk-only: refused java/foo/Bar");
        }
        Verdict::Agree => panic!("a strict refusal on stderr must be gated"),
    }
}

#[test]
fn for_mode_gates_only_the_four_profile_modes() {
    for &m in Mode::all() {
        let n = Normalizer::for_mode(m);
        assert_eq!(
            n.gate_stderr,
            m.collects_census(),
            "{} stderr gating",
            m.label()
        );
        if m.collects_census() {
            assert_eq!(n, Normalizer::jdk_only(), "{}", m.label());
        } else {
            assert_eq!(n, Normalizer::strict(), "{}", m.label());
        }
    }
}

#[test]
fn strict_selects_only_line_ending_hygiene() {
    let n = Normalizer::strict();
    let ids: Vec<&str> = n.active_rules().iter().map(|r| r.id).collect();
    assert_eq!(ids, vec!["line-endings"]);
    assert_eq!(n.active_stderr_rules().len(), 1);
    // Every opt-in knob is off, including the three added for the C2 review.
    assert!(!n.mask_timestamps);
    assert!(!n.normalize_path_separators);
    assert!(!n.mask_frame_line_numbers);
    assert!(n.describe().contains("line-endings"));
    assert!(n.describe().contains("ungated"));
}

#[test]
fn jdk_only_selects_the_two_stderr_rules_in_order() {
    let n = Normalizer::jdk_only();
    let ids: Vec<&str> = n.active_stderr_rules().iter().map(|r| r.id).collect();
    // ANSI must precede vm-diagnostics: the tracing lines are colourised,
    // so a match on `cratonvm_` only works once the escapes are gone.
    assert_eq!(ids, vec!["ansi", "vm-diagnostics", "line-endings"]);
    // The stderr-only pair never touches stdout.
    let shared: Vec<&str> = n.active_rules().iter().map(|r| r.id).collect();
    assert_eq!(shared, vec!["line-endings"]);
    assert!(n.describe().contains("stderr gated"));
}

#[test]
fn each_knob_selects_exactly_its_own_rules() {
    let cases: [(Normalizer, &[&str]); 6] = [
        (
            Normalizer {
                mask_hashes: true,
                ..Normalizer::strict()
            },
            &["line-endings", "identity-hash", "hex-address"],
        ),
        (
            Normalizer {
                mask_thread_ids: true,
                ..Normalizer::strict()
            },
            &["line-endings", "thread-id"],
        ),
        (
            Normalizer {
                mask_timestamps: true,
                ..Normalizer::strict()
            },
            &["line-endings", "timestamp"],
        ),
        (
            Normalizer {
                normalize_path_separators: true,
                ..Normalizer::strict()
            },
            &["line-endings", "path-separator"],
        ),
        (
            Normalizer {
                strip_paths: true,
                ..Normalizer::strict()
            },
            &["line-endings", "absolute-path"],
        ),
        (
            Normalizer {
                mask_frame_line_numbers: true,
                ..Normalizer::strict()
            },
            &["line-endings", "frame-line-numbers"],
        ),
    ];
    for (n, expected) in cases {
        let mut ids: Vec<&str> = n.active_rules().iter().map(|r| r.id).collect();
        let mut want: Vec<&str> = expected.to_vec();
        ids.sort_unstable();
        want.sort_unstable();
        assert_eq!(ids, want, "{}", n.describe());
    }
}

#[test]
fn an_opted_in_rule_neutralizes_only_its_target_in_a_real_comparison() {
    // End to end: two runs that differ *only* in an identity hash agree
    // under `mask_hashes`, and a run that differs in a real value still
    // diverges under the same normalizer.
    let n = Normalizer {
        mask_hashes: true,
        ..Normalizer::strict()
    };
    let a = obs("java.lang.Object@1b6d3586\nvalue=7", "", Some(0));
    let b = obs("java.lang.Object@7ffe0102\nvalue=7", "", Some(0));
    assert!(compare(&a, &b, &n).agrees());

    let c = obs("java.lang.Object@7ffe0102\nvalue=8", "", Some(0));
    assert_eq!(
        channels(&compare(&a, &c, &n)),
        vec![Channel::Stdout],
        "masking the hash must not mask the value"
    );
    // …and under the strict normalizer the hash difference is a divergence
    // again, which is what makes the rule an explicit opt-in.
    assert!(!compare(&a, &b, &Normalizer::strict()).agrees());
}

#[test]
fn gated_eq_ignores_stderr_unless_the_normalizer_gates_it() {
    let a = obs("42", "[cratonvm] chatter", Some(0));
    // Both must be *VM* chatter for the profile normalizer to strip both;
    // `strip_vm_diagnostics` keys on the `[cratonvm]`/`[NativeBridge]`
    // markers, so an unmarked line survives and is compared.
    let b = obs("42", "[cratonvm] different chatter entirely", Some(0));
    assert!(gated_eq(&a, &b, &Normalizer::strict()));
    // Under the profile normalizer, chatter is stripped from both, so they
    // still match — drift is judged on what's left.
    assert!(gated_eq(&a, &b, &Normalizer::jdk_only()));
    let c = obs("42", "a real message", Some(0));
    assert!(!gated_eq(&a, &c, &Normalizer::jdk_only()));
    assert!(gated_eq(&a, &c, &Normalizer::strict()));
}

#[test]
fn strip_ansi_removes_csi_sequences_only() {
    assert_eq!(strip_ansi("\u{1b}[2mdim\u{1b}[0m"), "dim");
    assert_eq!(strip_ansi("plain"), "plain");
    assert_eq!(strip_ansi("\u{1b}[38;5;196mred\u{1b}[0m!"), "red!");
    // Brackets that aren't escapes stay put.
    assert_eq!(strip_ansi("[cratonvm] ok"), "[cratonvm] ok");
}

// -- classification ranking ----------------------------------------------

fn violating(mode: Mode, diverged: bool) -> ModeVerdict {
    ModeVerdict {
        jdk_only_violation: true,
        ..mv(mode, diverged)
    }
}

#[test]
fn classify_jdk_only_violation_outranks_crash_but_not_hang() {
    // Rank: Hang > JdkOnlyViolation > Crash > JitOnly > GcMode > Universal.
    let mut crash = violating(Mode::JdkOnlyJit, true);
    crash.crashed = true;
    assert_eq!(classify(&[crash]), Some(Classification::JdkOnlyViolation));

    let mut hang = violating(Mode::JdkOnlyJit, true);
    hang.timed_out = true;
    assert_eq!(classify(&[hang]), Some(Classification::Hang));
}

#[test]
fn classify_jdk_only_violation_outranks_the_bisection_labels() {
    // A JIT-shaped bisection that also tripped the policy is reported as
    // the policy finding: the violation names the cause.
    let modes = [
        violating(Mode::JdkOnlyJit, true),
        mv(Mode::JdkOnlyNoJit, false),
    ];
    assert_eq!(classify(&modes), Some(Classification::JdkOnlyViolation));
}

#[test]
fn classify_violation_on_an_agreeing_run_is_still_reported() {
    // Wave 1 is measurement (contract §10): a violation that did not change
    // behaviour is still recorded. The gate's exit code does not move —
    // that is `GateReport::strict_violations`' job, not this label's.
    let modes = [violating(Mode::JdkOnlyJit, false)];
    assert_eq!(classify(&modes), Some(Classification::JdkOnlyViolation));
}

#[test]
fn classify_compatible_modes_never_report_a_violation() {
    // A compatible run records *would-be* refusals, which are never a
    // violation — so the historical labels are untouched.
    let modes = [mv(Mode::JitOn, true), mv(Mode::NoJit, false)];
    assert_eq!(classify(&modes), Some(Classification::JitOnly));
}
