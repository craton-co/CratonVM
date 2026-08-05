// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `java/lang/String` native surface, and which of it survives into a
//! real-JDK registry.
//!
//! # What this file used to assert, and why it changed
//!
//! WP8.10.9 registered `String.contains(CharSequence)` and
//! `String.startsWith(String,int)` because synthetic-jdk mode raised
//! `NoSuchMethodError` the moment a boot path called
//! `someName.contains("Module")` (the canonical case is
//! `vm/tests/wildfly_boot_fixtures/JBossModulesProbe.java`). This file then
//! asserted those triples were present in the registry of a
//! `VmConfig::default()` VM — which, in the default build (no `synthetic-jdk`
//! feature), is a **real-JDK** registry. The requirement was about synthetic
//! mode; the assertion was taken in the other one.
//!
//! On 2026-08-04 the forced-native `java/lang/String` policy was removed
//! (`docs/internal/forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`).
//! Contract §1.4 — real class bytes are authoritative — is now enforced where
//! it can be enforced once for every dispatch path: `NativeMethodRegistry::
//! register` drops every `java/lang/String` `Bridge` in real-JDK mode, so the
//! real `String.contains` bytecode runs. The WP8.10.9 requirement is untouched:
//! synthetic mode does not set that drop, and the registrations are still made.
//!
//! So this file now asserts the policy rather than the registration, in the one
//! mode it can observe:
//!
//! 1. the `java/lang/String` `Bridge` surface is ABSENT from a real-JDK
//!    registry — including `contains` and `startsWith`, the two WP8.10.9
//!    named;
//! 2. `intern()` survives, because the image declares it `ACC_NATIVE` and a
//!    bridge in front of a genuinely native method is contract §1.5, not a
//!    shadow;
//! 3. the four reviewed `NativeKind::Intrinsic` fast-regex shapes survive,
//!    because §1.4's reviewed exception is decided on kind and needs no name
//!    list at any dispatch site.
//!
//! (2) and (3) are what stop (1) from being a test that a whole class was
//! deleted: the rule is "drop the bridges that shadow bytecode", and a rule
//! that dropped everything would satisfy (1) just as well.

use std::sync::Arc;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::SharedVm;

fn shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

/// Every `java/lang/String` shape whose real-JDK class bytes carry a `Code`
/// attribute must lose — and be gone from the registry entirely, so no
/// dispatch path has anything to find.
///
/// The list is the shapes the four deleted copies of the policy forced, plus
/// the two WP8.10.9 named.
#[test]
fn real_jdk_registry_has_no_string_bridge_shadowing_bytecode() {
    let shared = shared();
    let registry = &shared.natives.native_methods;

    let shadowing: &[(&str, &str)] = &[
        // WP8.10.9's two.
        ("contains", "(Ljava/lang/CharSequence;)Z"),
        ("startsWith", "(Ljava/lang/String;I)Z"),
        // The h2-bnf five.
        ("substring", "(I)Ljava/lang/String;"),
        ("charAt", "(I)C"),
        ("length", "()I"),
        ("isEmpty", "()Z"),
        ("startsWith", "(Ljava/lang/String;)Z"),
        // The quadratic-parent substring fix.
        ("substring", "(II)Ljava/lang/String;"),
        // The charset-name constructors — the FOURTH copy of the policy, which
        // the record counted as three. They were also wrong: `new
        // String(bytes, "NO-SUCH")` returned a UTF-8 decode instead of raising
        // `UnsupportedEncodingException`, and `"US-ASCII"` decoded as Latin-1.
        ("<init>", "([BLjava/lang/String;)V"),
        ("<init>", "([BIILjava/lang/String;)V"),
        // The Unicode/locale-sensitive residue the cold list forced.
        ("trim", "()Ljava/lang/String;"),
        ("toLowerCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
        ("toUpperCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
        // The plain ones it also forced.
        ("equals", "(Ljava/lang/Object;)Z"),
        ("endsWith", "(Ljava/lang/String;)Z"),
        ("indexOf", "(Ljava/lang/String;)I"),
        ("lastIndexOf", "(Ljava/lang/String;)I"),
        ("replace", "(CC)Ljava/lang/String;"),
        ("toString", "()Ljava/lang/String;"),
    ];

    for &(name, descriptor) in shadowing {
        assert!(
            registry.find("java/lang/String", name, descriptor).is_none(),
            "java/lang/String.{name}{descriptor} is registered in a real-JDK registry again. \
             The JDK 25 image declares it with a `Code` attribute, so a registered `Bridge` in \
             front of it is contract §1.4's `NativeShadowsBytecode` — and, measured against \
             HotSpot with `probes/StringPolicyMatrixProbe`, these natives were the CAUSE of 57 \
             of 392 divergences (unpaired surrogates decoded to U+FFFD, out-of-range indices \
             with no exception message, `null` arguments answered with a default instead of an \
             NPE). Re-registering one reinstates that. If it must win, review it against the \
             probe and register it `NativeKind::Intrinsic`."
        );
    }
}

/// `intern()` survives the drop: the image declares it `ACC_NATIVE`, so there
/// is no bytecode for it to shadow.
///
/// Without this, the test above would pass just as happily for a change that
/// dropped the whole class — a different and much worse change.
#[test]
fn real_jdk_registry_keeps_the_one_genuine_string_bridge() {
    let shared = shared();
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/String", "intern", "()Ljava/lang/String;")
            .is_some(),
        "java/lang/String.intern()Ljava/lang/String; is gone. It is the ONE `java/lang/String` \
         registration the JDK 25 image declares `ACC_NATIVE` (79 of the other 80 carry a `Code` \
         attribute), so it is a legitimate §1.5 bridge and the real-JDK drop must not take it. \
         If this fails, the drop stopped being 'drop the bridges that shadow bytecode' and \
         became 'drop the class'."
    );
}

/// `String.hashCode()` survives the drop, and for a reason that is not speed.
///
/// The real `String.hashCode()` bytecode is WRONG on this VM for any string
/// whose backing array is UTF-16: it hashes the first `length()` BYTES of that
/// array, sign-extended to `char`, instead of the `length()` code units. The
/// object is fine — `length`, `charAt` and `equals` on it all agree with
/// HotSpot — so the defect is in what `hashCode` dispatches to. Measured with
/// `probes/StringUtf16HashProbe`; filed as
/// `docs/known-issues/string-utf16-hashcode-reads-bytes-not-code-units.md`.
///
/// Dropping this registration therefore replaces a correct answer with a wrong
/// one for every non-ASCII `String` key in the VM. When the `StringUTF16`
/// defect is fixed, re-measure and probably delete this registration: at that
/// point it is a pure performance optimisation again (the ~1950x caching win
/// it was originally written for) and has to argue on those terms.
#[test]
fn real_jdk_registry_keeps_string_hash_code_because_the_bytecode_is_wrong() {
    let shared = shared();
    assert_eq!(
        shared
            .natives
            .native_methods
            .kind_of("java/lang/String", "hashCode", "()I"),
        Some(cratonvm_native_api::NativeKind::Intrinsic),
        "java/lang/String.hashCode()I must survive the real-JDK `Bridge` drop, stated          `Intrinsic`. It is not kept for speed: the bytecode it would fall through to          hashes the backing BYTES sign-extended rather than the UTF-16 code units, so          `ΣΟΣ`.hashCode() returns 62956255 where the JLS (and HotSpot) say          924359 — while `charAt`/`length`/`equals` on the same object are all correct. See          docs/known-issues/string-utf16-hashcode-reads-bytes-not-code-units.md."
    );
}

/// The four reviewed `NativeKind::Intrinsic` fast-regex shapes survive.
///
/// They are how §1.4's reviewed exception is expressed now: on the KIND, with
/// no name list at any dispatch site. Registering them `Bridge` — including by
/// omission, since the ambient category at that registration site *is* `Bridge`
/// — silently deletes the SBR-02 fix, which is the failure this pins.
#[test]
fn real_jdk_registry_keeps_the_reviewed_string_intrinsics() {
    let shared = shared();
    let registry = &shared.natives.native_methods;

    // Same default-ON / opt-out reading as the registration site. With the flag
    // off the four are deliberately never registered and the real bytecode
    // runs, so there is nothing to assert — and saying so explicitly keeps a
    // `CRATONVM_NATIVE_STRING_REGEX=0` environment from turning this into a
    // failure that looks like a regression.
    let regex_natives_enabled =
        match cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_STRING_REGEX") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        };
    if !regex_natives_enabled {
        return;
    }

    let reviewed: &[(&str, &str)] = &[
        (
            "replaceAll",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "replaceFirst",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ),
        ("matches", "(Ljava/lang/String;)Z"),
        (
            "replace",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
        ),
    ];

    for &(name, descriptor) in reviewed {
        let kind = registry.kind_of("java/lang/String", name, descriptor);
        assert_eq!(
            kind,
            Some(cratonvm_native_api::NativeKind::Intrinsic),
            "java/lang/String.{name}{descriptor} must be registered `Intrinsic`, and is \
             {kind:?}. `Bridge` — which is the AMBIENT category at that registration site, so \
             it is what dropping `register_with_kind` gets you — means the real-JDK drop \
             removes it and the SBR-02 fast-regex fix disappears with no error anywhere. That \
             silent-deletion shape is what the forced-native `String` record exists about."
        );
    }
}
