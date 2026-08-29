// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(feature = "synthetic-jdk")]
// WP1.8 validates legacy synthetic-stub method-table closures. The default VM
// build boots real JDK classes and does not register those synthetic stubs.

//! WP1.8-narrow — close the two synthetic-stub method-table gaps that
//! prevent `java.util.ServiceLoader.load(Class).iterator()` from running
//! end-to-end.
//!
//! Roadmap reference: `gaps/wildfly-ejbca-roadmap.md` Wave 1 §4 (WP1.8).
//!
//! The two gaps documented in the WP7.1 commit (`245e996`) head comment
//! of `native-builtins/src/jdbc.rs`:
//!
//!   1. `java.lang.Class.forName(Ljava/lang/String;)Ljava/lang/Class;`
//!      — the synthetic `java/lang/Class` stub method table must declare
//!      it so bytecode resolution finds the method before the native
//!      registry is consulted.
//!   2. `java.io.BufferedReader.<init>(Ljava/io/Reader;)V` — the
//!      synthetic `java/io/BufferedReader` stub method table must declare
//!      the (Reader) constructor (the (Reader, int) overload was already
//!      reachable via the existing native registration).
//!
//! Acceptance:
//!
//! 1. **forName closure**: `vm.invoke("java/lang/Class", "forName",
//!    "(Ljava/lang/String;)Ljava/lang/Class;", &[Value::Object(string("java.lang.String"))])`
//!    returns a non-null Class.
//! 2. **BufferedReader closure**: a Java fixture that does
//!    `new BufferedReader(new StringReader(...))` does not throw
//!    `NoSuchMethodError`.
//! 3. **End-to-end**: a Java fixture that calls
//!    `ServiceLoader.load(java.sql.Driver.class).iterator()` and counts
//!    providers returns >0 when a `META-INF/services/java.sql.Driver`
//!    is on the classpath.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::native::register_builtins;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE_CLASS: &str = "cratonvm/Wp18ServiceLoaderE2E";

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_compiled() -> bool {
    let path = format!(
        "{}/cratonvm/Wp18ServiceLoaderE2E.class",
        test_resources_dir()
    );
    std::path::Path::new(&path).exists()
}

/// Materialize a temp directory containing
/// `META-INF/services/java.sql.Driver` listing the fixture's FakeDriver
/// FQN. Mirrors the WP7.1 helper but writes the WP1.8 fixture's FakeDriver
/// FQN.
fn make_spi_classpath_dir() -> std::path::PathBuf {
    let dir = tempfile::TempDir::new().expect("create temp dir for SPI fixture");
    let services_dir = dir.path().join("../../apps/META-INF").join("services");
    std::fs::create_dir_all(&services_dir).expect("create META-INF/services");
    let descriptor = services_dir.join("java.sql.Driver");
    std::fs::write(&descriptor, "cratonvm.Wp18ServiceLoaderE2E$FakeDriver\n")
        .expect("write META-INF/services/java.sql.Driver");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

fn vm_with_spi(spi_dir: &std::path::Path) -> Vm {
    let cp = vec![test_resources_dir(), spi_dir.to_string_lossy().into_owned()];
    let config = VmConfig::new().with_classpath(cp);
    Vm::new(config)
}

fn vm_no_spi() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

// ---------------------------------------------------------------------------
// Functional 1 — forName closure
// ---------------------------------------------------------------------------

/// Registry-side anchor: the `Class.forName(Ljava/lang/String;)Ljava/lang/Class;`
/// native must be wired into the registry under the proper descriptor.
/// Catches a regression where the registration is dropped or moved to a
/// non-essential phase, leaving bytecode-side `Class.forName(String)` on
/// the synthetic stub with no fallback.
#[test]
fn class_for_name_string_native_registered() {
    use cratonvm_native_api::NativeMethodRegistry;
    let mut r = NativeMethodRegistry::new();
    register_builtins(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "forName",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some(),
        "Class.forName(String) native must be registered for the synthetic-stub \
         method-table declaration to dispatch.",
    );
}

/// Bytecode-side closure: from real Java bytecode,
/// `Class.forName("java.lang.String")` resolves and dispatches.
/// Returns -99 if a Throwable was caught (typical regression: a
/// NoSuchMethodError because the synthetic stub method table omitted
/// the declaration).
#[test]
fn class_for_name_resolves_from_bytecode() {
    if !fixture_compiled() {
        eprintln!("Skipping: Wp18ServiceLoaderE2E.class not available (javac not on PATH?)");
        return;
    }
    let mut vm = vm_no_spi();
    let result = vm.invoke(FIXTURE_CLASS, "forNameStringResolves", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        Ok(Some(Value::Int(-99))) => panic!(
            "forNameStringResolves caught a Throwable — Class.forName(String) \
             still throws (NoSuchMethodError or other) at bytecode \
             resolution. Did the synthetic-stub method-table addition \
             land?",
        ),
        other => panic!("forNameStringResolves expected Ok(Some(Int(1))), got: {other:?}",),
    }
}

// ---------------------------------------------------------------------------
// Functional 2 — BufferedReader closure
// ---------------------------------------------------------------------------

/// Bytecode-side closure: `new BufferedReader(new StringReader("hello"))`
/// must not throw `NoSuchMethodError`. The (Reader) constructor must be
/// resolvable through the synthetic stub method table.
#[test]
fn buffered_reader_reader_ctor_resolves_from_bytecode() {
    if !fixture_compiled() {
        eprintln!("Skipping: Wp18ServiceLoaderE2E.class not available (javac not on PATH?)");
        return;
    }
    let mut vm = vm_no_spi();
    let result = vm.invoke(FIXTURE_CLASS, "bufferedReaderCtorResolves", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        Ok(Some(Value::Int(-99))) => panic!(
            "bufferedReaderCtorResolves caught a Throwable — \
             BufferedReader.<init>(Reader) still throws \
             NoSuchMethodError at bytecode resolution. Did the \
             synthetic-stub method-table addition land?",
        ),
        other => panic!("bufferedReaderCtorResolves expected Ok(Some(Int(1))), got: {other:?}",),
    }
}

// ---------------------------------------------------------------------------
// End-to-end — ServiceLoader.iterator returns >0
// ---------------------------------------------------------------------------

/// Sanity guard: the FakeDriver class itself loads + instantiates.
#[test]
fn fake_driver_instantiates_directly() {
    if !fixture_compiled() {
        eprintln!("Skipping: Wp18ServiceLoaderE2E.class not available (javac not on PATH?)");
        return;
    }
    let mut vm = vm_no_spi();
    let result = vm.invoke(FIXTURE_CLASS, "fakeDriverInstantiates", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        other => panic!("fakeDriverInstantiates expected Ok(Some(Int(1))), got: {other:?}",),
    }
}

/// WP1.8-narrow acceptance: a driver advertised in
/// `META-INF/services/java.sql.Driver` is reported by
/// `ServiceLoader.load(java.sql.Driver.class).iterator()` end-to-end
/// (no native helpers, no shortcut paths).
#[test]
fn service_loader_iterator_discovers_driver() {
    if !fixture_compiled() {
        eprintln!("Skipping: Wp18ServiceLoaderE2E.class not available (javac not on PATH?)");
        return;
    }
    let spi_dir = make_spi_classpath_dir();
    let mut vm = vm_with_spi(&spi_dir);
    let result = vm.invoke(FIXTURE_CLASS, "serviceLoaderIteratorCount", "()I", &[]);
    match result {
        Ok(Some(Value::Int(n))) if n > 0 => {}
        Ok(Some(Value::Int(0))) => panic!(
            "ServiceLoader.iterator() returned 0 — descriptor not \
             walked or providers not instantiated. Check the \
             service_loader.rs::discover_providers + \
             native_sl_iterator chain.",
        ),
        Ok(Some(Value::Int(-99))) => panic!(
            "ServiceLoader fixture caught a Throwable — \
             check Class.forName / BufferedReader.<init> resolution.",
        ),
        other => panic!("serviceLoaderIteratorCount expected Ok(Some(Int(n>0))), got: {other:?}",),
    }
}
