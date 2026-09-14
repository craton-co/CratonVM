// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.1-narrow — `Class.getDeclaredMethods()` end-to-end dispatch.
//!
//! Roadmap reference: `wildfly-ejbca-roadmap.md` §5 (Wave 2 — WP2.1).
//!
//! # What this suite anchors
//!
//! Closes the synthetic-stub method-table gap that prevented
//! `java.lang.Class.getDeclaredMethods()` from surfacing methods for
//! synthetic JDK-interface stubs (Connection, ResultSet, etc.). The
//! gap manifested at the reflection-layer's synthetic-method-decls
//! table in `native-builtins/src/lang_class.rs`: the `getDeclaredMethods0`
//! native was registered, but the table in `synthetic_jdk_method_decls`
//! had no entries for the JDBC SPI types, so the native returned empty
//! arrays and frameworks holding real JDBC bytecode (HikariCP, Spring
//! JDBC, Hibernate connection proxies) saw an empty introspection
//! surface.
//!
//! This is a *narrow* acceptance — the full WP2.1 surface (annotation
//! reflection, generic types, method invoke fast path) lives in the
//! sibling `wp2_1_reflect.rs` and `wp2_2_method_invoke_matrix.rs`
//! suites. Here we pin exactly the gap that the WP7.2 best-effort
//! tests called out as "log SKIP today" — once this gap closed, those
//! tests promote to hard PASS.
//!
//! # Test layout
//!
//! 1. `class_get_declared_methods_native_registered` —
//!    same shape as the WP7.2 anchor: native registry pin.
//!    Cheap regression net so any future re-shuffle of the registration
//!    site catches the gap before the fixture runs.
//! 2. `connection_get_declared_methods_returns_non_empty` — Java
//!    fixture probe that `Connection.class.getDeclaredMethods()` lands
//!    a non-empty array AND every Method has a non-null `getName()` +
//!    `toString()`.
//! 3. `result_set_next_round_trips_through_method_get_return_type` —
//!    `Method.getReturnType()` round-trip pin: ResultSet's `next()`
//!    must reflect as returning `boolean.class`.
//! 4. `class_get_declared_methods_closure` — composite end-to-end
//!    probe; the load-bearing acceptance proof.
//! 5. `fixture_class_file_is_staged` — anchor-grep style fixture guard.

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::native::register_builtins;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE_CLASS: &str = "cratonvm/Wp21ClassReflectE2E";

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_compiled() -> bool {
    let path = format!(
        "{}/cratonvm/Wp21ClassReflectE2E.class",
        test_resources_dir()
    );
    std::path::Path::new(&path).exists()
}

fn fresh_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

fn run_probe(method: &str) -> Result<i32, String> {
    let mut vm = fresh_vm();
    match vm.invoke(FIXTURE_CLASS, method, "()I", &[]) {
        Ok(Some(Value::Int(n))) => Ok(n),
        Ok(other) => Err(format!("unexpected return value: {other:?}")),
        Err(e) => Err(format!("vm.invoke failed: {e:?}")),
    }
}

// ---------------------------------------------------------------------------
// Registry-side anchors — cheap regressions if the registration site moves.
// ---------------------------------------------------------------------------

#[test]
fn class_get_declared_methods_native_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getDeclaredMethods0",
            "(Z)[Ljava/lang/reflect/Method;"
        )
        .is_some(),
        "WP2.1-narrow: getDeclaredMethods0 native must be in essential registry"
    );
}

/// Synthetic-mode-only — the public `getDeclaredMethods()` registration
/// lives in `register_synthetic_overrides` because real-JDK mode would
/// run the real bytecode. This pin guards against accidental removal.
#[test]
#[cfg(feature = "synthetic-jdk")]
fn class_get_declared_methods_public_native_registered_synthetic() {
    let mut r = NativeMethodRegistry::new();
    register_builtins(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getDeclaredMethods",
            "()[Ljava/lang/reflect/Method;"
        )
        .is_some(),
        "WP2.1-narrow: public Class.getDeclaredMethods() must be registered \
         under synthetic-jdk so bytecode dispatch lands the native"
    );
}

// ---------------------------------------------------------------------------
// Functional probes — Java fixture drives the end-to-end path.
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "synthetic-jdk")]
fn connection_get_declared_methods_returns_non_empty() {
    if !fixture_compiled() {
        eprintln!(
            "Skipping connection_get_declared_methods_returns_non_empty: \
             Wp21ClassReflectE2E.class not staged (javac unavailable?)"
        );
        return;
    }

    let n = run_probe("connectionDeclaredMethodCount")
        .expect("WP2.1-narrow: Connection.class.getDeclaredMethods probe must invoke cleanly");
    assert!(
        n > 0,
        "WP2.1-narrow: Connection.class.getDeclaredMethods() must surface ≥1 \
         method (got {n}). \
         A negative value indicates the failure mode: \
           -1 throwable, -2 null array, -3 null name, -4 null toString."
    );
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn result_set_next_round_trips_through_method_get_return_type() {
    if !fixture_compiled() {
        eprintln!(
            "Skipping result_set_next_round_trips_through_method_get_return_type: \
             fixture not staged"
        );
        return;
    }

    let r = run_probe("resultSetNextReturnsBoolean")
        .expect("WP2.1-narrow: ResultSet.next return-type probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-narrow: ResultSet.class.getDeclaredMethods() must surface a \
         next() method whose getReturnType() is boolean.class"
    );
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn class_get_declared_methods_closure() {
    if !fixture_compiled() {
        eprintln!("Skipping class_get_declared_methods_closure: fixture not staged");
        return;
    }

    let r = run_probe("closesGetDeclaredMethodsGap")
        .expect("WP2.1-narrow: composite gap-closure probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-narrow acceptance failed: \
         Connection.class.getDeclaredMethods() and \
         ResultSet.class.getDeclaredMethods() must both return non-empty \
         arrays of Method objects with non-null name + toString. \
         If this regresses, check `synthetic_jdk_method_decls` in \
         `native-builtins/src/lang_class.rs`."
    );
}

// ---------------------------------------------------------------------------
// Fixture staging guard.
// ---------------------------------------------------------------------------

#[test]
fn fixture_class_file_is_staged() {
    let java = format!("{}/cratonvm/Wp21ClassReflectE2E.java", test_resources_dir());
    let class = format!(
        "{}/cratonvm/Wp21ClassReflectE2E.class",
        test_resources_dir()
    );
    assert!(
        std::path::Path::new(&java).exists(),
        "Wp21ClassReflectE2E.java fixture must exist at {java}"
    );
    if !std::path::Path::new(&class).exists() {
        eprintln!(
            "WARN: {class} not staged — javac missing at build time. \
             Functional probes will skip; registry-gate tests still run."
        );
    }
}
