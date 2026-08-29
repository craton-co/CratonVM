// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK-only mode — configuration and diagnostics
//! (`docs/feature-designs/jdk-only-mode.md` §3, §6 and §1.7).
//!
//! Two things are pinned here, and they are related.
//!
//! **Configuration.** `CompatibilityMode` is orthogonal to `JdkMode`: one
//! selects which class library boots, the other selects which substitutions
//! are permitted inside it. Strict mode is reachable only by an explicit
//! request — never from a Cargo feature, an environment variable, or what the
//! host happens to have installed. This repository already made the
//! host-derived-mode mistake once with `use_synthetic_jdk = detect_real_jdk()
//! .is_none()`, where the same binary and the same command line ran a
//! different standard library depending on the machine; these tests exist so
//! the enforcement policy cannot repeat it.
//!
//! **Diagnostics.** A strict-mode refusal is only useful if the operator can
//! act on it, so the rendered violation must name the class, the method *with
//! its descriptor* (an overload is not identified without one), the reason,
//! the JDK feature version, and the `--real-jdk` fallback — and it must not
//! leak the build agent's absolute paths into a pasted bug report unless
//! `--explain-jdk-only` asked for them.
//!
//! No JDK image, no fixture and no Cargo feature: every assertion is a pure
//! function of values constructed here, so this file runs in the default build
//! on every platform.

use cratonvm_types::error::JdkOnlyViolation;
use cratonvm_vm::config::{
    CompatibilityMode, ExecutionPolicy, JdkMode, VmConfig, EMBEDDED_DEFAULT_COMPATIBILITY_MODE,
    LAUNCHER_DEFAULT_COMPATIBILITY_MODE,
};
use cratonvm_vm::error::VmError;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[test]
fn launcher_default_is_real_jdk_plus_compatible() {
    let cfg = VmConfig::for_launcher();
    assert_eq!(
        cfg.jdk_mode(),
        JdkMode::Real,
        "the launcher boots the real class library by default"
    );
    assert_eq!(
        cfg.compatibility_mode,
        CompatibilityMode::Compatible,
        "…under today's substitution policy. `--jdk-only` is opt-in: a run \
         whose enforcement policy was chosen for it is not reproducible"
    );
    assert!(!cfg.is_jdk_only());
    assert_eq!(
        cfg.execution_policy(),
        ExecutionPolicy::compatible(true),
        "the policy token handed to the registry, the class manager and the \
         dispatch resolver must agree with the config it came from"
    );
    assert!(
        cfg.validate_compatibility().is_ok(),
        "the default configuration must always be coherent"
    );
}

#[test]
fn embedded_default_is_compatible_too() {
    // The embedded default deliberately *differs* from the launcher on
    // `JdkMode` (synthetic, so an embedder needs no JDK) but deliberately
    // *agrees* on `CompatibilityMode`. Flipping the embedded default to strict
    // would change the semantics of every in-tree test that never asked for it.
    let cfg = VmConfig::default();
    assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
    assert!(!cfg.is_jdk_only());
    assert_eq!(
        LAUNCHER_DEFAULT_COMPATIBILITY_MODE,
        CompatibilityMode::Compatible
    );
    assert_eq!(
        EMBEDDED_DEFAULT_COMPATIBILITY_MODE,
        CompatibilityMode::Compatible
    );
}

#[test]
fn compatibility_mode_default_impl_is_the_permissive_one() {
    assert_eq!(CompatibilityMode::default(), CompatibilityMode::Compatible);
    assert_eq!(
        ExecutionPolicy::default(),
        ExecutionPolicy::compatible(true)
    );
}

// ---------------------------------------------------------------------------
// `--jdk-only` maps to real + strict
// ---------------------------------------------------------------------------

#[test]
fn jdk_only_selects_real_jdk_and_strict_policy() {
    // What the CLI's `--jdk-only` branch is specified to build (contract §9):
    // `JdkMode::Real` + `CompatibilityMode::JdkOnly`.
    let cfg = VmConfig::for_launcher()
        .with_jdk_mode(JdkMode::Real)
        .with_compatibility_mode(CompatibilityMode::JdkOnly);

    assert!(cfg.is_jdk_only());
    assert_eq!(cfg.jdk_mode(), JdkMode::Real);
    assert_eq!(cfg.execution_policy(), ExecutionPolicy::jdk_only());
    assert!(cfg.execution_policy().real_jdk);
    assert!(cfg.execution_policy().is_jdk_only());
    assert!(
        cfg.validate_compatibility().is_ok(),
        "real + strict is the whole point of the mode"
    );
}

#[test]
fn bare_real_jdk_stays_compatible() {
    // `--real-jdk` is NOT a weaker `--jdk-only`; it is today's behaviour named
    // explicitly, and it is what the strict-mode error messages point at as
    // the fallback.
    let cfg = VmConfig::for_launcher().with_jdk_mode(JdkMode::Real);
    assert!(!cfg.is_jdk_only());
    assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
}

// ---------------------------------------------------------------------------
// Rejection: strict + synthetic
// ---------------------------------------------------------------------------

#[test]
fn strict_plus_synthetic_library_is_a_configuration_error() {
    let cfg = VmConfig::for_launcher()
        .with_jdk_mode(JdkMode::Synthetic)
        .with_compatibility_mode(CompatibilityMode::JdkOnly);

    let err = cfg.validate_compatibility().expect_err(
        "--jdk-only means real class bytes are authoritative; the \
             synthetic library is built entirely out of the substitutions the \
             mode exists to forbid, so there would be no class library left to \
             run",
    );
    let msg = match err {
        VmError::InvalidConfiguration(m) => m,
        other => panic!(
            "the conflict is the operator's input being contradictory, not a \
             VM bug — it must be InvalidConfiguration, got {other:?}"
        ),
    };
    assert!(
        msg.contains("--jdk-only"),
        "the error must name the flag that caused it: {msg}"
    );
    assert!(
        msg.contains("--synthetic-jdk"),
        "…and the flag it conflicts with: {msg}"
    );
    assert!(
        msg.contains("--real-jdk"),
        "…and how to get out of it: {msg}"
    );
}

#[test]
fn strict_on_the_embedded_default_is_rejected_too() {
    // `VmConfig::default()` is synthetic, so an embedder that sets only
    // `compatibility_mode` gets the same coherent refusal rather than a
    // NoClassDefFoundError storm at first class load.
    let cfg = VmConfig::default().with_compatibility_mode(CompatibilityMode::JdkOnly);
    assert!(matches!(
        cfg.validate_compatibility(),
        Err(VmError::InvalidConfiguration(_))
    ));
}

#[test]
fn every_other_combination_validates() {
    for (jdk, compat) in [
        (JdkMode::Real, CompatibilityMode::Compatible),
        (JdkMode::Real, CompatibilityMode::JdkOnly),
        (JdkMode::Synthetic, CompatibilityMode::Compatible),
    ] {
        let cfg = VmConfig::for_launcher()
            .with_jdk_mode(jdk)
            .with_compatibility_mode(compat);
        assert!(
            cfg.validate_compatibility().is_ok(),
            "{jdk:?} + {compat:?} must be accepted; strict+synthetic is the \
             ONLY rejected pair"
        );
    }
}

// ---------------------------------------------------------------------------
// Round-tripping (embedding surface)
// ---------------------------------------------------------------------------

#[test]
fn compatibility_mode_round_trips_through_the_embedding_surface() {
    for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
        // Builder form.
        let built = VmConfig::default().with_compatibility_mode(mode);
        assert_eq!(built.compatibility_mode, mode);
        assert_eq!(built.is_jdk_only(), mode.is_jdk_only());
        assert_eq!(built.execution_policy().compatibility_mode, mode);

        // Public-field form: the field is `pub`, and an embedder that assigns
        // it directly must get the same answers as the builder.
        let mut assigned = VmConfig::default();
        assigned.compatibility_mode = mode;
        assert_eq!(assigned.compatibility_mode, built.compatibility_mode);
        assert_eq!(assigned.is_jdk_only(), built.is_jdk_only());
        assert_eq!(assigned.execution_policy(), built.execution_policy());
    }
}

#[test]
fn setting_compatibility_mode_does_not_silently_rewrite_the_jdk_mode() {
    // Selecting the class library and selecting the substitution policy are
    // separate decisions. Having `with_compatibility_mode` quietly flip
    // `use_synthetic_jdk` would recreate exactly the implicit mode change
    // `JdkMode` was cleaned up to remove — and would hide the misconfiguration
    // that `validate_compatibility` exists to report.
    let cfg = VmConfig::default();
    let before = cfg.jdk_mode();
    let after = cfg.with_compatibility_mode(CompatibilityMode::JdkOnly);
    assert_eq!(
        after.jdk_mode(),
        before,
        "with_compatibility_mode must not touch the JDK mode"
    );
    assert!(
        after.validate_compatibility().is_err(),
        "…which is precisely what makes the incoherent pair reportable"
    );
}

#[test]
fn compatibility_mode_spellings_are_the_wire_format() {
    // These strings appear in the `--jdk-only-report` `mode` field, in the
    // difftest ledger key and in log lines. Re-spelling one silently
    // invalidates every committed artifact keyed on it.
    assert_eq!(CompatibilityMode::Compatible.as_str(), "compatible");
    assert_eq!(CompatibilityMode::JdkOnly.as_str(), "jdk-only");
    assert!(!CompatibilityMode::Compatible.is_jdk_only());
    assert!(CompatibilityMode::JdkOnly.is_jdk_only());
}

#[test]
fn execution_policy_constructors_agree_with_their_fields() {
    assert_eq!(
        ExecutionPolicy::compatible(true),
        ExecutionPolicy {
            compatibility_mode: CompatibilityMode::Compatible,
            real_jdk: true,
        }
    );
    assert_eq!(
        ExecutionPolicy::compatible(false),
        ExecutionPolicy {
            compatibility_mode: CompatibilityMode::Compatible,
            real_jdk: false,
        }
    );
    assert_eq!(
        ExecutionPolicy::jdk_only(),
        ExecutionPolicy {
            compatibility_mode: CompatibilityMode::JdkOnly,
            real_jdk: true,
        },
        "jdk_only() implies a real JDK — the mode is meaningless without one"
    );
    assert!(!ExecutionPolicy::compatible(true).is_jdk_only());
    assert!(ExecutionPolicy::jdk_only().is_jdk_only());
}

// ---------------------------------------------------------------------------
// Diagnostics: stable, actionable, descriptor-bearing, path-safe
// ---------------------------------------------------------------------------

fn sample_violations() -> Vec<JdkOnlyViolation> {
    vec![
        JdkOnlyViolation::CompatibilityClassRequested {
            class: "org/jboss/logging/Logger".to_string(),
            initiating_loader: Some("jdk/internal/loader/ClassLoaders$AppClassLoader".to_string()),
            requester: Some("com/example/App.main([Ljava/lang/String;)V".to_string()),
            reason: "no real class bytes on the classpath or module path".to_string(),
        },
        JdkOnlyViolation::SyntheticNativeRegistered {
            class: "java/util/HashMap".to_string(),
            method: "put".to_string(),
            descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
            registered_by: Some("native-builtins/src/lib.rs:1234".to_string()),
        },
        JdkOnlyViolation::SyntheticNativeInvocation {
            class: "java/util/HashMap".to_string(),
            method: "get".to_string(),
            descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
            call_site: Some("com/example/App.run()V".to_string()),
        },
        JdkOnlyViolation::MissingNative {
            class: "sun/nio/ch/Net".to_string(),
            method: "socket0".to_string(),
            descriptor: "(ZZZZ)I".to_string(),
            module: Some("java.base".to_string()),
        },
        JdkOnlyViolation::NativeShadowsBytecode {
            class: "java/lang/String".to_string(),
            method: "indexOf".to_string(),
            descriptor: "(I)I".to_string(),
            native_kind: "bridge",
        },
        JdkOnlyViolation::MissingBootClass {
            class: "java/lang/Object".to_string(),
            searched_image: "/opt/jdk-25".to_string(),
        },
        JdkOnlyViolation::MissingImplementation {
            class: "com/example/Abstract".to_string(),
            method: "compute".to_string(),
            descriptor: "()J".to_string(),
        },
    ]
}

#[test]
fn violation_kind_tags_are_distinct_and_stable() {
    let violations = sample_violations();
    let mut kinds: Vec<&str> = violations.iter().map(|v| v.kind()).collect();
    let n = kinds.len();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(
        kinds.len(),
        n,
        "each variant needs its own counter key; duplicates collapse two \
         failure modes into one line of the report"
    );

    // Spelled out because they are the report's wire format.
    let by_kind: Vec<&str> = violations.iter().map(|v| v.kind()).collect();
    assert_eq!(
        by_kind,
        vec![
            "compatibility-class-requested",
            "synthetic-native-registered",
            "synthetic-native-invocation",
            "missing-native",
            "native-shadows-bytecode",
            "missing-boot-class",
            "missing-implementation",
        ]
    );
}

#[test]
fn every_violation_names_its_class_and_method_descriptor() {
    for v in sample_violations() {
        let summary = v.summary();
        let rendered = v.render(Some(25), false);
        let json = v.to_json();

        assert!(
            !summary.is_empty(),
            "Display/summary must never be empty: {v:?}"
        );
        assert_eq!(
            format!("{v}"),
            summary,
            "Display is specified to be summary()"
        );

        // Whatever the variant, the class it is about appears in all three
        // renderings — that is the minimum an operator needs to start.
        let class = match &v {
            JdkOnlyViolation::CompatibilityClassRequested { class, .. }
            | JdkOnlyViolation::SyntheticNativeRegistered { class, .. }
            | JdkOnlyViolation::SyntheticNativeInvocation { class, .. }
            | JdkOnlyViolation::MissingNative { class, .. }
            | JdkOnlyViolation::NativeShadowsBytecode { class, .. }
            | JdkOnlyViolation::MissingBootClass { class, .. }
            | JdkOnlyViolation::MissingImplementation { class, .. } => class.clone(),
        };
        assert!(
            summary.contains(&class),
            "summary lost the class: {summary}"
        );
        assert!(rendered.contains(&class), "render lost the class");
        assert!(json.contains(&class), "to_json lost the class");

        // The method-shaped variants must carry the descriptor everywhere.
        // Without it an overload cannot be identified, and "HashMap.put is a
        // stub" is not an actionable statement.
        if let JdkOnlyViolation::SyntheticNativeRegistered { descriptor, .. }
        | JdkOnlyViolation::SyntheticNativeInvocation { descriptor, .. }
        | JdkOnlyViolation::MissingNative { descriptor, .. }
        | JdkOnlyViolation::NativeShadowsBytecode { descriptor, .. }
        | JdkOnlyViolation::MissingImplementation { descriptor, .. } = &v
        {
            assert!(
                summary.contains(descriptor.as_str()),
                "summary lost the descriptor: {summary}"
            );
            assert!(
                rendered.contains(descriptor.as_str()),
                "render lost the descriptor"
            );
            assert!(
                json.contains(descriptor.as_str()),
                "to_json lost the descriptor"
            );
        }
    }
}

#[test]
fn render_always_ends_with_the_real_jdk_fallback_and_the_report_hint() {
    // Contract §3: the long form ends with the `--real-jdk` fallback. This is
    // the line that tells an operator their build is not broken, only refused,
    // so nothing may be appended after it except the capture hint.
    for v in sample_violations() {
        let rendered = v.render(Some(25), true);
        let mut lines = rendered.lines().filter(|l| !l.trim().is_empty());
        let last_two: Vec<&str> = {
            let all: Vec<&str> = lines.by_ref().collect();
            all[all.len() - 2..].to_vec()
        };
        assert!(
            last_two[0].contains("--real-jdk"),
            "the penultimate line must offer the fallback, got {last_two:?}"
        );
        assert!(
            last_two[1].contains("--jdk-only-report"),
            "the final line must offer whole-run capture, got {last_two:?}"
        );
        assert!(
            rendered.starts_with("CratonVM --jdk-only:"),
            "the headline must say which mode refused: {rendered}"
        );
    }
}

#[test]
fn render_reports_the_jdk_feature_version() {
    let v = &sample_violations()[3]; // MissingNative
    assert!(
        v.render(Some(25), false).contains("25"),
        "the JDK feature version is part of every strict-mode failure report \
         (contract §1.7): the same method is native in one release and \
         bytecode in the next"
    );
    assert!(
        v.render(None, false).contains("<unknown>"),
        "an unknown feature version must say so rather than be omitted"
    );
    assert!(
        v.render(Some(25), false).contains("java.base"),
        "the owning module is what turns 'implement this native' into a \
         work item against a specific JDK module"
    );
}

#[test]
fn render_reports_the_class_provenance_for_a_fabrication_refusal() {
    // The origin *category* itself (`boot-image`, `vm-array`,
    // `compatibility-stub`, …) is asserted against `ClassOrigin::as_str` in
    // `classloading/tests/jdk_only_class_origin.rs`, which owns that enum.
    // What the violation must carry is the per-request provenance: who asked,
    // through which loader, and why it was refused.
    let v = &sample_violations()[0];
    let rendered = v.render(Some(25), false);
    assert!(
        rendered.contains("jdk/internal/loader/ClassLoaders$AppClassLoader"),
        "the initiating loader is half the answer to 'why did this class not \
         resolve': {rendered}"
    );
    assert!(
        rendered.contains("com/example/App.main"),
        "the requester names the code that needs fixing: {rendered}"
    );
    assert!(
        rendered.contains("no real class bytes"),
        "the reason must survive into the long form: {rendered}"
    );
}

#[test]
fn absolute_paths_are_redacted_unless_verbose() {
    for image in ["/opt/jenkins/agent/jdk-25", "C:\\Users\\build\\jdk-25"] {
        let v = JdkOnlyViolation::MissingBootClass {
            class: "java/lang/Object".to_string(),
            searched_image: image.to_string(),
        };

        let quiet = v.render(Some(25), false);
        assert!(
            !quiet.contains(image),
            "reports get pasted into issue trackers; a full java.home leaks the \
             operator's home directory, the build-agent layout and often a \
             customer name. Leaked {image} in:\n{quiet}"
        );
        // This file was recovered onto `dev` on 2026-07-31 from an auto-commit
        // in another checkout, and it has been red ever since. It asserted that
        // redaction keeps the last path component ("jdk-25"), on the reasoning
        // that the tail is what identifies the JDK. That is a defensible
        // design, but it is not the one that shipped, and two other places say
        // so: `types/src/error.rs`'s own test asserts
        // `redact_paths(path, false) == "<redacted>"` for a whole token, and
        // `docs/jdk-only-migration.md` shows `java.home: <redacted>`.
        //
        // Whole-token redaction wins here because the two specs differ in what
        // LEAKS, and absent a decision the conservative one should hold: a
        // trailing component is `jdk-25` on a release build and
        // `acme-prod-2026` on a customer's. The information is not lost — the
        // renderer now tells you to re-run with `--explain-jdk-only`, which is
        // the assertion below and the part of the original intent that was
        // genuinely missing.
        assert!(
            quiet.contains("<redacted>"),
            "an absolute path must be redacted as a whole token:\n{quiet}"
        );
        assert!(
            quiet.contains("--explain-jdk-only"),
            "a redacted report must say how to get the full paths:\n{quiet}"
        );

        let loud = v.render(Some(25), true);
        assert!(
            loud.contains(image),
            "--explain-jdk-only prints the path untouched:\n{loud}"
        );
    }
}

#[test]
fn relative_provenance_is_never_redacted() {
    // A registration site like `native-builtins/src/lib.rs:1234` is already
    // safe and is the single most useful field in the report. Over-eager
    // redaction here would delete the answer.
    let v = JdkOnlyViolation::SyntheticNativeRegistered {
        class: "java/util/HashMap".to_string(),
        method: "put".to_string(),
        descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
        registered_by: Some("native-builtins/src/lib.rs:1234".to_string()),
    };
    assert!(v
        .render(Some(25), false)
        .contains("native-builtins/src/lib.rs:1234"));
}

#[test]
fn to_json_is_a_single_object_with_a_kind_and_no_absent_keys() {
    for v in sample_violations() {
        let json = v.to_json();
        assert!(
            json.starts_with('{') && json.ends_with('}'),
            "to_json returns one complete object so the report can join them \
             with commas: {json}"
        );
        // Compact separator, no space: `types/src/error.rs` owns this dump and
        // its own test pins `"kind":"…"`. The difference is cosmetic and the
        // format is machine-read, so the crate that emits it keeps the say.
        assert!(
            json.contains(&format!("\"kind\":\"{}\"", v.kind())),
            "every entry is self-describing: {json}"
        );
        assert!(
            json.contains("\"class\":"),
            "every entry names a class: {json}"
        );
        assert!(
            !json.contains(",,") && !json.contains("{,") && !json.contains(",}"),
            "hand-rolled JSON must not emit an empty element: {json}"
        );
    }

    // Absent optionals are `null`, not omitted: a consumer that sees no key
    // cannot tell "not recorded" from "the schema changed".
    let v = JdkOnlyViolation::MissingNative {
        class: "sun/nio/ch/Net".to_string(),
        method: "socket0".to_string(),
        descriptor: "(ZZZZ)I".to_string(),
        module: None,
    };
    assert!(
        v.to_json().contains("\"module\":null"),
        "got {}",
        v.to_json()
    );
}

#[test]
fn to_json_escapes_the_characters_a_descriptor_can_contain() {
    // Descriptors and class names are attacker-adjacent input in the sense
    // that they come from a class file, not from us. A `"` or `\` in a name
    // must not be able to break the report out of its string.
    let v = JdkOnlyViolation::MissingImplementation {
        class: "com/example/We\"ird".to_string(),
        method: "m\\n".to_string(),
        descriptor: "()V".to_string(),
    };
    let json = v.to_json();
    assert!(json.contains("We\\\"ird"), "got {json}");
    assert!(json.contains("m\\\\n"), "got {json}");
}

#[test]
fn a_violation_converts_into_a_vm_error_without_losing_itself() {
    let v = JdkOnlyViolation::MissingNative {
        class: "sun/nio/ch/Net".to_string(),
        method: "socket0".to_string(),
        descriptor: "(ZZZZ)I".to_string(),
        module: Some("java.base".to_string()),
    };
    let err: VmError = v.clone().into();
    match err {
        VmError::JdkOnly(inner) => assert_eq!(inner, v),
        other => panic!("expected VmError::JdkOnly, got {other:?}"),
    }
}
