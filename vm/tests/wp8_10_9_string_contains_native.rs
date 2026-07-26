// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP8.10.9 — `String.contains(CharSequence)` (and `startsWith(String,I)`)
//! native registration regression.
//!
//! Without these registrations, synthetic-jdk mode raises
//! `NoSuchMethodError` the moment any boot path calls
//! `someName.contains("Module")` (the canonical case is
//! `vm/tests/wildfly_boot_fixtures/JBossModulesProbe.java::probeJBossModuleClassReachable`).
//!
//! Acceptance:
//!
//! 1. The hot triple
//!    `(java/lang/String, contains, (Ljava/lang/CharSequence;)Z)`
//!    is present in `shared.natives.native_methods` after VM construction.
//! 2. The same is true for the offset overload of `startsWith`.
//! 3. The registered native produces the spec-correct result for the
//!    typical hits/misses + the empty-needle edge case.
//!
//! Companion: see WP8.10.5 (`is_jdk_class` extension that unblocked
//! this NSME path) and the probe0 assertion in
//! `vm/tests/wp8_10_jboss_modules_smoke.rs:202`.

use std::sync::Arc;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{create_java_string, NativeContextImpl, SharedVm, Vm};

fn shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

#[test]
fn string_contains_charsequence_is_registered() {
    let shared = shared();
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/lang/String",
                "contains",
                "(Ljava/lang/CharSequence;)Z"
            )
            .is_some(),
        "WP8.10.9 regression: String.contains(CharSequence) MUST be \
         registered in `register_essential_natives` so synthetic-jdk \
         mode does not NSME on `name.contains(\"Module\")` from \
         WildFly / JBoss Modules boot."
    );
}

#[test]
fn string_starts_with_offset_is_registered() {
    let shared = shared();
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/String", "startsWith", "(Ljava/lang/String;I)Z")
            .is_some(),
        "WP8.10.9 regression: the (String, int) overload of String.startsWith \
         MUST be registered alongside the single-arg variant — bytecode that \
         does `s.startsWith(\"x\", 4)` would otherwise NSME."
    );
}

#[test]
fn string_contains_true_via_dispatch() {
    let mut vm = Vm::new(VmConfig::default());
    let haystack = create_java_string(&vm.shared, "org.jboss.modules.Module");
    let needle = create_java_string(&vm.shared, "Module");

    let cb = vm
        .shared
        .natives
        .native_methods
        .find(
            "java/lang/String",
            "contains",
            "(Ljava/lang/CharSequence;)Z",
        )
        .expect("contains(CharSequence) must be registered");
    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };
    let r = cb(
        &mut ctx,
        &[Value::Object(Some(haystack)), Value::Object(Some(needle))],
    )
    .expect("contains call should not error");
    assert_eq!(
        r,
        Some(Value::Int(1)),
        "\"...Module\".contains(\"Module\") must be true"
    );
}

#[test]
fn string_contains_false_via_dispatch() {
    let mut vm = Vm::new(VmConfig::default());
    let haystack = create_java_string(&vm.shared, "hello world");
    let needle = create_java_string(&vm.shared, "xyz");

    let cb = vm
        .shared
        .natives
        .native_methods
        .find(
            "java/lang/String",
            "contains",
            "(Ljava/lang/CharSequence;)Z",
        )
        .expect("contains(CharSequence) must be registered");
    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };
    let r = cb(
        &mut ctx,
        &[Value::Object(Some(haystack)), Value::Object(Some(needle))],
    )
    .expect("contains call should not error");
    assert_eq!(
        r,
        Some(Value::Int(0)),
        "\"hello world\".contains(\"xyz\") must be false"
    );
}

#[test]
fn string_contains_empty_needle_is_true() {
    // JDK spec: every String contains the empty string.
    let mut vm = Vm::new(VmConfig::default());
    let haystack = create_java_string(&vm.shared, "anything");
    let empty = create_java_string(&vm.shared, "");

    let cb = vm
        .shared
        .natives
        .native_methods
        .find(
            "java/lang/String",
            "contains",
            "(Ljava/lang/CharSequence;)Z",
        )
        .expect("contains(CharSequence) must be registered");
    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };
    let r = cb(
        &mut ctx,
        &[Value::Object(Some(haystack)), Value::Object(Some(empty))],
    )
    .expect("contains call should not error");
    assert_eq!(
        r,
        Some(Value::Int(1)),
        "\"anything\".contains(\"\") must be true per JDK String spec"
    );
}

#[test]
fn string_starts_with_offset_via_dispatch() {
    let mut vm = Vm::new(VmConfig::default());
    let s = create_java_string(&vm.shared, "cratonvm.boot.fixture");
    let prefix = create_java_string(&vm.shared, "boot");

    let cb = vm
        .shared
        .natives
        .native_methods
        .find("java/lang/String", "startsWith", "(Ljava/lang/String;I)Z")
        .expect("startsWith(String, int) must be registered");
    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };
    // "cratonvm.boot.fixture".startsWith("boot", 9) == true
    let r = cb(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Object(Some(prefix)),
            Value::Int(9),
        ],
    )
    .expect("startsWith call should not error");
    assert_eq!(r, Some(Value::Int(1)));

    // Off the end → false (not OOBE — that variant per JDK returns false
    // when offset > length()).
    let r2 = cb(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Object(Some(prefix)),
            Value::Int(100),
        ],
    )
    .expect("startsWith call should not error on out-of-range offset");
    assert_eq!(r2, Some(Value::Int(0)));
}
