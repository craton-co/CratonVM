// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! real-cdi-bean-container increment 1 — interface static-final initialization.
//!
//! A `static final` field declared on an INTERFACE whose initializer is NOT a
//! compile-time constant carries no `ConstantValue` attribute (JVMS §4.7.2);
//! its value is assigned by the interface's `<clinit>`. Per JVMS §5.5, the VM
//! must run that `<clinit>` lazily on the first `getstatic` that reads the
//! field. If the interface `<clinit>` is skipped, the field reads back as its
//! prepared default (`0` for `int`) instead of the computed value.
//!
//! This is the *general* VM gap that the Spring `ApplicationStartup.DEFAULT`
//! shim (`native-builtins/src/spring_startup_bootstrap.rs`) used to paper over:
//! `ApplicationStartup.DEFAULT` is exactly a non-constant `static final` on the
//! `ApplicationStartup` interface. This test pins the general behavior with a
//! minimal, framework-independent fixture so the fix is regression-tested in
//! isolation from Spring.
//!
//! Fixture: `cratonvm/IfaceStaticFinalInit` (real-JDK bytecode, JDK 25,
//! class major 69) — see `vm/tests/resources/cratonvm/IfaceStaticFinalInit.java`.
//! `IntHolder.VALUE = compute()` is runtime-computed (== 42), so it is NOT a
//! `ConstantValue`. `probeInterfaceStaticFinal()` does a single
//! `getstatic IntHolder.VALUE`. The assertion (== 42, not the default 0) proves
//! the interface `<clinit>` ran on first access.

#![cfg(not(feature = "synthetic-jdk"))]

use cratonvm_types::Value;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!(
        "{dir}/cratonvm/IfaceStaticFinalInit$IntHolder.class"
    ))
    .exists()
}

/// Resolve a real JDK 25 install (the VM still needs `java.base` for `Object`,
/// `String`, etc. when running app fixtures). Mirrors `management_factory_clinit.rs`.
fn java_home() -> Option<std::path::PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = std::path::PathBuf::from(&jh);
        if p.join("lib").join("modules").exists() {
            return Some(p);
        }
    }
    let default =
        std::path::PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default.join("lib").join("modules").exists() {
        return Some(default);
    }
    None
}

/// Accessing a non-constant `static final` field on an interface must run the
/// interface `<clinit>` and observe the computed value, not the prepared default.
#[test]
fn interface_non_constant_static_final_is_initialized_on_first_access() {
    if !fixture_available() {
        eprintln!("Skipping: IfaceStaticFinalInit fixture not compiled");
        return;
    }
    let Some(jh) = java_home() else {
        eprintln!(
            "Skipping: real JDK 25 not found. Set JAVA_HOME or install \
             Adoptium at the default path."
        );
        return;
    };

    let config = VmConfig::new()
        .with_java_home(jh.to_string_lossy().into_owned())
        .with_classpath(vec![test_resources_dir()]);
    let mut vm = Vm::new(config);

    // `probeInterfaceStaticFinal()` does a single `getstatic IntHolder.VALUE`.
    // IntHolder has never been touched, so this getstatic is what must trigger
    // the interface's `<clinit>` (VALUE = compute() == 42).
    let result = vm.invoke(
        "cratonvm/IfaceStaticFinalInit",
        "probeInterfaceStaticFinal",
        "()I",
        &[],
    );

    let value = match result {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("probeInterfaceStaticFinal() did not return an int: {other:?}"),
    };

    assert_eq!(
        value, 42,
        "interface non-constant static-final VALUE read back as {value} — \
         expected 42. A value of 0 means the interface `<clinit>` was skipped \
         on first `getstatic` (the general gap the Spring ApplicationStartup \
         shim used to hide)."
    );
}
