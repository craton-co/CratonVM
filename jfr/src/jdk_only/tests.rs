use super::*;

fn violation_of_each_kind() -> Vec<JdkOnlyViolation> {
    vec![
        JdkOnlyViolation::CompatibilityClassRequested {
            class: "org/jboss/Secret".into(),
            initiating_loader: Some("app".into()),
            requester: Some("com/acme/Boot.main([Ljava/lang/String;)V".into()),
            reason: "enterprise-prefix fallback".into(),
            class_origin: Some("compatibility-stub".into()),
        },
        JdkOnlyViolation::SyntheticNativeRegistered {
            class: "java/lang/Foo".into(),
            method: "bar".into(),
            descriptor: "()V".into(),
            registered_by: Some("native-builtins/src/lib.rs:1234".into()),
            survivor: None,
        },
        JdkOnlyViolation::SyntheticNativeInvocation {
            class: "java/lang/Foo".into(),
            method: "bar".into(),
            descriptor: "()V".into(),
            call_site: Some("com/acme/App.run()V".into()),
        },
        JdkOnlyViolation::MissingNative {
            class: "java/lang/Foo".into(),
            method: "baz".into(),
            descriptor: "()V".into(),
            module: Some("java.base".into()),
        },
        JdkOnlyViolation::NativeShadowsBytecode {
            class: "java/lang/Foo".into(),
            method: "qux".into(),
            descriptor: "()V".into(),
            native_kind: "bridge",
            class_origin: Some("boot-image".into()),
        },
        JdkOnlyViolation::MissingBootClass {
            class: "java/lang/Object".into(),
            searched_image: "C:\\jdk-25\\lib\\modules".into(),
        },
        JdkOnlyViolation::MissingImplementation {
            class: "java/lang/Foo".into(),
            method: "quux".into(),
            descriptor: "()V".into(),
        },
    ]
}

// ── Vocabularies are exactly the enum spellings ───────────────────────

#[test]
fn violation_kind_vocabulary_is_exactly_the_enum() {
    let kinds: Vec<&str> = violation_of_each_kind().iter().map(|v| v.kind()).collect();
    assert_eq!(
        kinds, VIOLATION_KINDS,
        "VIOLATION_KINDS must mirror JdkOnlyViolation::kind(), in declaration order"
    );
    assert_eq!(VIOLATION_KINDS.len(), N_VIOLATION);
}

/// `ClassOrigin::as_str()` lives in `classloading`, which this crate does
/// not depend on, so the mirror is asserted against the contract's own
/// list (design doc §5) rather than against the enum. The tags are a wire
/// format; re-spelling one is the failure this guards.
#[test]
fn class_origin_vocabulary_is_the_ten_contract_tags() {
    assert_eq!(
        CLASS_ORIGINS,
        &[
            "boot-image",
            "application-class-path",
            "user-defined",
            "vm-array",
            "hidden-class",
            "generated-lambda",
            "generated-proxy",
            "reflection-accessor",
            "vm-internal",
            "compatibility-stub",
        ]
    );
    assert_eq!(CLASS_ORIGINS.len(), N_ORIGIN);
}

#[test]
fn native_kind_vocabulary_is_the_three_contract_tags() {
    assert_eq!(NATIVE_KINDS, &["intrinsic", "bridge", "synthetic-stub"]);
    assert_eq!(NATIVE_KINDS.len(), N_NATIVE_KIND);
}

#[test]
fn generators_are_a_subset_of_class_origins() {
    for g in GENERATORS {
        assert!(
            CLASS_ORIGINS.contains(g),
            "{g} is not a ClassOrigin tag — the generator vocabulary must not fork"
        );
    }
    assert_eq!(GENERATORS.len(), N_GENERATOR);
    // The two VM-created-but-not-generated origins stay out.
    assert!(generator_label("vm-array").is_none());
    assert!(generator_label("vm-internal").is_none());
    assert!(generator_label("boot-image").is_none());
}

#[test]
fn module_allow_list_is_sorted_and_sized() {
    let mut sorted = JDK_MODULES.to_vec();
    sorted.sort_unstable();
    assert_eq!(
        sorted, JDK_MODULES,
        "JDK_MODULES must be sorted for binary_search"
    );
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        JDK_MODULES.len(),
        "JDK_MODULES must be unique"
    );
    assert_eq!(JDK_MODULES.len() + 2, N_MODULE);
}

#[test]
fn every_counter_name_is_the_planned_spelling() {
    assert_eq!(
        COUNTER_NAMES,
        &[
            "cratonvm_jdk_only_violation_total",
            "cratonvm_class_origin_total",
            "cratonvm_native_registration_total",
            "cratonvm_native_invocation_total",
            "cratonvm_real_bytecode_shadow_attempt_total",
            "cratonvm_missing_native_total",
            "cratonvm_generated_class_total",
        ]
    );
}

// ── Disabled by default, and inert when disabled ──────────────────────

#[test]
fn default_is_disabled() {
    assert!(!JdkOnlyTelemetry::default().is_enabled());
    assert!(!JdkOnlyCounters::default().is_enabled());
}

#[test]
fn disabled_counters_are_zero_and_inert() {
    let mut t = JdkOnlyTelemetry::default();
    t.add_violations(violation_of_each_kind().iter());
    t.add_class_origins(CLASS_ORIGINS.iter().copied());
    t.add_native_census(NATIVE_KINDS.iter().copied().map(|k| NativeCensusSample {
        kind: k,
        invocations: 99,
    }));
    let c = t.finish();

    assert!(!c.is_enabled());
    assert_eq!(c.unrecognised_tags(), 0);
    assert_eq!(c.real_bytecode_shadow_attempt_total(), 0);
    assert_eq!(c.missing_native_grand_total(), 0);
    assert_eq!(c.generated_class_grand_total(), 0);
    for k in VIOLATION_KINDS {
        assert_eq!(c.violation_total(k), 0, "{k}");
    }
    for o in CLASS_ORIGINS {
        assert_eq!(c.class_origin_total(o), 0, "{o}");
    }
    for k in NATIVE_KINDS {
        assert_eq!(c.native_registration_total(k), 0, "{k}");
        assert_eq!(c.native_invocation_total(k), 0, "{k}");
    }
    assert_eq!(c, JdkOnlyCounters::default());
}

#[test]
fn disabled_json_is_all_zero_but_full_shape() {
    let json = JdkOnlyCounters::default().to_json();
    assert!(json.contains("\"enabled\": false"));
    // Shape is stable even with nothing measured: every label present.
    for k in VIOLATION_KINDS {
        assert!(json.contains(&format!("\"{k}\": 0")), "missing label {k}");
    }
    for m in JDK_MODULES {
        assert!(json.contains(&format!("\"{m}\": 0")), "missing module {m}");
    }
    assert!(json.contains("\"unknown\": 0"));
    assert!(json.contains("\"other\": 0"));
}

// ── Enabled aggregation ───────────────────────────────────────────────

#[test]
fn violations_fold_into_kind_shadow_and_module_counters() {
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_violations(violation_of_each_kind().iter());
    let c = t.finish();

    for k in VIOLATION_KINDS {
        assert_eq!(c.violation_total(k), 1, "{k}");
    }
    assert_eq!(c.real_bytecode_shadow_attempt_total(), 1);
    assert_eq!(c.missing_native_total("java.base"), 1);
    assert_eq!(c.missing_native_grand_total(), 1);
    assert_eq!(c.unrecognised_tags(), 0);
}

#[test]
fn class_origins_fold_into_origin_and_generator_counters() {
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_class_origins([
        "boot-image",
        "boot-image",
        "generated-lambda",
        "generated-proxy",
        "hidden-class",
        "reflection-accessor",
        "vm-array",
        "vm-internal",
        "compatibility-stub",
    ]);
    let c = t.finish();

    assert_eq!(c.class_origin_total("boot-image"), 2);
    assert_eq!(c.class_origin_total("compatibility-stub"), 1);
    // Four generators, one each; vm-array and vm-internal excluded.
    assert_eq!(c.generated_class_grand_total(), 4);
    assert_eq!(c.generated_class_total("generated-lambda"), 1);
    assert_eq!(c.generated_class_total("hidden-class"), 1);
    // …but the §9 fold does include them.
    assert_eq!(c.contract_counts().generated_classes, 6);
}

#[test]
fn native_census_folds_registrations_and_invocations_in_one_pass() {
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_native_census([
        NativeCensusSample {
            kind: "bridge",
            invocations: 100,
        },
        NativeCensusSample {
            kind: "bridge",
            // A superseded row reports 0; it still counts as a registration.
            invocations: 0,
        },
        NativeCensusSample {
            kind: "intrinsic",
            invocations: 7,
        },
        NativeCensusSample {
            kind: "synthetic-stub",
            invocations: 0,
        },
    ]);
    let c = t.finish();

    assert_eq!(c.native_registration_total("bridge"), 2);
    assert_eq!(c.native_invocation_total("bridge"), 100);
    assert_eq!(c.native_registration_total("intrinsic"), 1);
    assert_eq!(c.native_invocation_total("intrinsic"), 7);
    assert_eq!(c.native_registration_total("synthetic-stub"), 1);
    assert_eq!(c.native_invocation_total("synthetic-stub"), 0);
}

#[test]
fn class_bucket_fold_is_a_partition_of_the_origin_counter() {
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_class_origins(CLASS_ORIGINS.iter().copied());
    let c = t.finish();
    let counts = c.contract_counts();
    let bucketed = counts.boot_image_classes
        + counts.application_classes
        + counts.generated_classes
        + counts.compatibility_classes;
    let total: u64 = c.class_origins().map(|(_, v)| v).sum();
    assert_eq!(
        bucketed, total,
        "the four §9 buckets must partition the ten origin tags"
    );
}

#[test]
fn snapshot_does_not_consume_and_does_not_double_count() {
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_class_origins(["boot-image"]);
    let first = t.snapshot();
    t.add_class_origins(["boot-image"]);
    let second = t.snapshot();
    assert_eq!(first.class_origin_total("boot-image"), 1);
    assert_eq!(second.class_origin_total("boot-image"), 2);
}

// ── Label bounding ────────────────────────────────────────────────────

#[test]
fn unknown_module_is_bounded_to_other_and_absent_to_unknown() {
    assert_eq!(module_label(None), MODULE_LABEL_UNKNOWN);
    assert_eq!(module_label(Some("java.base")), "java.base");
    assert_eq!(module_label(Some("com.acme.internal")), MODULE_LABEL_OTHER);
    assert_eq!(
        module_label(Some("/home/victor/secret/app.jar")),
        MODULE_LABEL_OTHER
    );
    assert_eq!(
        module_label(Some("C:\\Users\\Victor\\keys")),
        MODULE_LABEL_OTHER
    );
    assert_eq!(module_label(Some("-Dpassword=hunter2")), MODULE_LABEL_OTHER);
    assert_eq!(module_label(Some("")), MODULE_LABEL_OTHER);
}

#[test]
fn missing_native_label_cardinality_is_bounded() {
    let mut t = JdkOnlyTelemetry::enabled();
    for i in 0..5_000u32 {
        t.add_violation(&JdkOnlyViolation::MissingNative {
            class: "x/Y".into(),
            method: "m".into(),
            descriptor: "()V".into(),
            module: Some(format!("attacker.module.{i}")),
        });
    }
    let c = t.finish();
    // 5000 distinct inputs, one label.
    assert_eq!(c.missing_native_total(MODULE_LABEL_OTHER), 5_000);
    assert_eq!(c.missing_native_grand_total(), 5_000);
    let labels: Vec<&str> = c.missing_natives().map(|(l, _)| l).collect();
    assert_eq!(labels.len(), N_MODULE);
    assert!(!c.to_json().contains("attacker"));
}

/// The privacy rule, stated as a test: feed violations whose every string
/// field is a path, a class name, a command-line argument or an
/// environment value, and assert that none of them reaches the JSON.
#[test]
fn no_path_or_argument_like_string_can_reach_a_label() {
    let secrets = [
        "/home/victor/apps/spring-boot.jar",
        "C:\\craton\\wt-jdk-only\\target\\release\\cratonvm.exe",
        "native-builtins/src/lib.rs:1234",
        "-Djavax.net.ssl.trustStorePassword=hunter2",
        "com/acme/PatientRecordDao",
        "AWS_SECRET_ACCESS_KEY=AKIA0000",
    ];
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_violation(&JdkOnlyViolation::CompatibilityClassRequested {
        class: secrets[4].into(),
        initiating_loader: Some(secrets[0].into()),
        requester: Some(secrets[1].into()),
        reason: secrets[3].into(),
        class_origin: Some(secrets[0].into()),
    });
    t.add_violation(&JdkOnlyViolation::SyntheticNativeRegistered {
        class: secrets[4].into(),
        method: "m".into(),
        descriptor: "()V".into(),
        registered_by: Some(secrets[2].into()),
        survivor: None,
    });
    t.add_violation(&JdkOnlyViolation::MissingNative {
        class: secrets[4].into(),
        method: "m".into(),
        descriptor: "()V".into(),
        module: Some(secrets[5].into()),
    });
    t.add_violation(&JdkOnlyViolation::MissingBootClass {
        class: secrets[4].into(),
        searched_image: secrets[1].into(),
    });
    t.add_class_origins([secrets[0], secrets[4]]);
    t.add_native_census([NativeCensusSample {
        kind: secrets[2],
        invocations: 1,
    }]);

    let counters = t.finish();
    let json = counters.to_json();
    for s in secrets {
        assert!(
            !json.contains(s),
            "{s} leaked into the counter block:\n{json}"
        );
    }
    // Nothing path-like at all: no separators, no drive letters, no
    // property assignments. (A single `\` does occur, in the escaped
    // `kind=\"bridge\"` of the lower-bound series, so the check is for the
    // two-character drive prefix rather than for the backslash itself.)
    assert!(!json.contains('/'), "a path separator reached the JSON");
    assert!(!json.contains(":\\"), "a drive letter reached the JSON");
    assert!(!json.contains("-D"), "a -D property reached the JSON");

    // Stronger than "the secrets are absent": every label the snapshot can
    // emit is a member of one of the closed vocabularies.
    let known = |l: &str| {
        VIOLATION_KINDS.contains(&l)
            || CLASS_ORIGINS.contains(&l)
            || NATIVE_KINDS.contains(&l)
            || GENERATORS.contains(&l)
            || JDK_MODULES.contains(&l)
            || l == MODULE_LABEL_OTHER
            || l == MODULE_LABEL_UNKNOWN
    };
    let emitted = counters
        .violations()
        .chain(counters.class_origins())
        .chain(counters.native_registrations())
        .chain(counters.native_invocations())
        .chain(counters.missing_natives())
        .chain(counters.generated_classes());
    for (label, _) in emitted {
        assert!(known(label), "{label} is not in any closed vocabulary");
    }
}

#[test]
fn unrecognised_tags_are_counted_never_echoed() {
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_class_origins(["not-a-real-origin", "/etc/passwd"]);
    t.add_native_census([NativeCensusSample {
        kind: "not-a-real-kind",
        invocations: 42,
    }]);
    let c = t.finish();
    assert_eq!(c.unrecognised_tags(), 3);
    // The unknown native row contributes no invocations anywhere.
    let total: u64 = c.native_invocations().map(|(_, v)| v).sum();
    assert_eq!(total, 0);
    let json = c.to_json();
    assert!(!json.contains("not-a-real"));
    assert!(!json.contains("passwd"));
}

// ── Versioning and JSON shape ─────────────────────────────────────────

#[test]
fn version_is_present_and_stable() {
    assert_eq!(JDK_ONLY_TELEMETRY_SCHEMA_VERSION, 1);
    let c = JdkOnlyTelemetry::enabled().finish();
    assert_eq!(c.schema_version(), JDK_ONLY_TELEMETRY_SCHEMA_VERSION);
    assert!(c.to_json().contains(&format!(
        "\"counter_schema_version\": {JDK_ONLY_TELEMETRY_SCHEMA_VERSION}"
    )));
}

#[test]
fn json_shape_is_run_to_run_stable() {
    let empty = JdkOnlyTelemetry::enabled().finish().to_json();
    let mut t = JdkOnlyTelemetry::enabled();
    t.add_violations(violation_of_each_kind().iter());
    t.add_class_origins(CLASS_ORIGINS.iter().copied());
    let busy = t.finish().to_json();
    // Same number of lines and the same keys in the same order: two CI
    // artefacts differ on values only.
    assert_eq!(empty.lines().count(), busy.lines().count());
    let keys = |s: &str| -> Vec<String> {
        s.lines()
            .filter_map(|l| l.split(':').next().map(|k| k.trim().to_string()))
            .collect()
    };
    assert_eq!(keys(&empty), keys(&busy));
}

#[test]
fn json_names_every_counter() {
    let json = JdkOnlyTelemetry::enabled().finish().to_json();
    for name in COUNTER_NAMES {
        assert!(json.contains(&format!("\"{name}\"")), "missing {name}");
    }
}

#[test]
fn lower_bound_is_emitted_as_data() {
    let c = JdkOnlyTelemetry::enabled().finish();
    assert!(c.is_lower_bound(COUNTER_NATIVE_INVOCATION_TOTAL, "bridge"));
    assert!(c.is_lower_bound(COUNTER_NATIVE_REGISTRATION_TOTAL, "bridge"));
    // The exact ones must not be labelled as floors.
    assert!(!c.is_lower_bound(COUNTER_NATIVE_INVOCATION_TOTAL, "synthetic-stub"));
    assert!(!c.is_lower_bound(COUNTER_NATIVE_INVOCATION_TOTAL, "intrinsic"));
    assert!(!c.is_lower_bound(COUNTER_VIOLATION_TOTAL, "missing-native"));

    let json = c.to_json();
    assert!(json.contains("\"lower_bound\""));
    assert!(json.contains("kind=\\\"bridge\\\""));
    assert!(json.contains("lower_bound_reason"));
    assert_eq!(c.lower_bound_labels().len(), 2);
}

// ── Opt-in ────────────────────────────────────────────────────────────

/// The gate is the existing §9 command-line surface, not a new environment
/// variable. If someone adds one, `types`'
/// `jdk_only_adds_no_environment_variable` fails too — this asserts the
/// other half: that the flags we gate on are real.
#[test]
fn enabling_flags_are_real_contract_flags() {
    for f in TELEMETRY_ENABLING_FLAGS {
        assert!(
            cratonvm_types::flag_groups::jdk_only_flag(f).is_some(),
            "{f} is not one of JDK_ONLY_CLI_FLAGS"
        );
        assert!(!f.starts_with("CRATONVM"), "{f} looks like an env var");
    }
}

#[test]
fn telemetry_is_off_unless_an_artefact_flag_is_present() {
    assert!(!enabled_by_args(["-cp", "app.jar", "com.acme.Main"]));
    // Selecting the policy is not asking to be measured.
    assert!(!enabled_by_args(["--real-jdk", "com.acme.Main"]));
    assert!(!enabled_by_args(["--explain-real-jdk"]));

    assert!(enabled_by_args([
        "--real-jdk",
        "--real-jdk-report",
        "r.json"
    ]));
    assert!(enabled_by_args(["--dump-class-origins=o.json"]));
    assert!(enabled_by_args(["--trace-real-jdk"]));

    assert!(!JdkOnlyTelemetry::new(false).is_enabled());
    assert!(JdkOnlyTelemetry::new(enabled_by_args(["--trace-real-jdk"])).is_enabled());
}

#[test]
fn near_miss_flags_do_not_open_the_gate() {
    assert!(!enabled_by_flag("--real-jdk-reports"));
    assert!(!enabled_by_flag("-real-jdk-report"));
    assert!(!enabled_by_flag(""));
    assert!(!enabled_by_flag("--dump-class-origin"));
}
