// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.1-class-modern — modern `java.lang.Class` reflection API surface.
//!
//! Roadmap reference: `wildfly-ejbca-roadmap.md` §5 Wave 2 — WP2.1.
//!
//! # What this suite anchors
//!
//! Closes the *Class* side of the WP2.1 hot list. The sibling
//! `wp2_1_reflect.rs` pins the public-API natives directly (registry
//! `find()` checks); this suite drives them end-to-end through Java
//! bytecode so framework-style call sites (ByteBuddy's
//! `TypeDescription.forLoadedType`, Hibernate's record/sealed scanners)
//! are exercised on real fixtures with sealed interfaces, records, and
//! nested classes.
//!
//! # Probes
//!
//! Each Java probe returns 1 on pass, 0 on fail; this harness asserts
//! `== 1`. The probes are:
//!
//! 1. `enclosingClassProbe`        — `Class.getEnclosingClass()`.
//! 2. `nestHostProbe`              — `Class.getNestHost()`.
//! 3. `permittedSubclassesProbe`   — `Class.getPermittedSubclasses()`.
//! 4. `recordComponentsProbe`      — `Class.getRecordComponents()`.
//! 5. `isSealedProbe`              — `Class.isSealed()` (sealed + plain).
//! 6. `getDeclaredMethodProbe`     — `Class.getDeclaredMethod(String, Class[])`.
//! 7. `getTypeAnnotationsProbe`    — `Class.getAnnotatedInterfaces()` /
//!    `getAnnotatedSuperclass()` non-null surface (best-effort).
//!
//! # Fixture
//!
//! `vm/tests/resources/cratonvm/Wp21ClassModern.java` contains a sealed
//! interface (`Shape` permits `Circle, Square, Triangle`), a record
//! (`Point(int x, int y)`), and nested classes (`Inner` / `Inner.Deeper`).
//! The fixture is marked `// JAVA21+` on line 1 so `vm/build.rs` compiles
//! it with `--release 21` (sealed classes need Java 17+; record patterns
//! and the sealed-class API stabilised in Java 17).

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::native::register_builtins;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE_CLASS: &str = "cratonvm/Wp21ClassModern";

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_compiled() -> bool {
    let path = format!("{}/cratonvm/Wp21ClassModern.class", test_resources_dir());
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
fn class_get_annotated_interfaces_native_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getAnnotatedInterfaces",
            "()[Ljava/lang/reflect/AnnotatedType;",
        )
        .is_some(),
        "WP2.1-class-modern: getAnnotatedInterfaces must be in essential registry"
    );
    assert!(
        r.find(
            "java/lang/Class",
            "getAnnotatedSuperclass",
            "()Ljava/lang/reflect/AnnotatedType;",
        )
        .is_some(),
        "WP2.1-class-modern: getAnnotatedSuperclass must be in essential registry"
    );
}

/// Anchor — the canonical natives backing the modern Class API are pinned.
///
/// `getDeclaredMethod(String, Class[])` is registered in
/// `register_synthetic_overrides` (because real-JDK mode runs the real
/// bytecode), so we use `register_builtins` here which calls both
/// essential + synthetic. The other three (`getNestHost0`,
/// `getPermittedSubclasses0`, `getRecordComponents0`) are unconditional
/// natives the real JDK delegates to from Java.
#[test]
#[cfg(feature = "synthetic-jdk")]
fn class_modern_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    register_builtins(&mut r);
    for (name, desc) in [
        ("getNestHost0", "()Ljava/lang/Class;"),
        ("getPermittedSubclasses0", "()[Ljava/lang/Class;"),
        (
            "getRecordComponents0",
            "()[Ljava/lang/reflect/RecordComponent;",
        ),
        (
            "getDeclaredMethod",
            "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        ),
    ] {
        assert!(
            r.find("java/lang/Class", name, desc).is_some(),
            "WP2.1-class-modern: java/lang/Class.{name}{desc} must be registered"
        );
    }
}

// ---------------------------------------------------------------------------
// Functional probes — Java fixture drives the end-to-end path.
// ---------------------------------------------------------------------------

fn run_probe_or_skip(method: &str, label: &str) {
    if !fixture_compiled() {
        eprintln!(
            "Skipping {label}: Wp21ClassModern.class not staged \
             (javac unavailable or pre-Java-21 toolchain?)"
        );
        return;
    }
    let r = run_probe(method)
        .unwrap_or_else(|e| panic!("WP2.1-class-modern {label} probe failed to invoke: {e}"));
    assert_eq!(
        r, 1,
        "WP2.1-class-modern {label} probe returned {r}; expected 1"
    );
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn enclosing_class_probe() {
    run_probe_or_skip("enclosingClassProbe", "getEnclosingClass()");
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn nest_host_probe() {
    run_probe_or_skip("nestHostProbe", "getNestHost()");
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn permitted_subclasses_probe() {
    run_probe_or_skip("permittedSubclassesProbe", "getPermittedSubclasses()");
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn record_components_probe() {
    run_probe_or_skip("recordComponentsProbe", "getRecordComponents()");
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn is_sealed_probe() {
    run_probe_or_skip("isSealedProbe", "isSealed()");
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn get_declared_method_probe() {
    run_probe_or_skip(
        "getDeclaredMethodProbe",
        "getDeclaredMethod(String, Class[])",
    );
}

/// `getAnnotatedInterfaces()` / `getAnnotatedSuperclass()` are best-effort
/// — RUNTIME-visible type annotations are not fully wired (that's WP2.7
/// territory). The non-null + non-throwing guarantee is what we anchor
/// here: frameworks introspecting `Class` need a non-null array even when
/// no annotations are present.
#[test]
#[cfg(feature = "synthetic-jdk")]
fn get_type_annotations_probe() {
    run_probe_or_skip(
        "getTypeAnnotationsProbe",
        "getAnnotatedInterfaces / getAnnotatedSuperclass",
    );
}

// ---------------------------------------------------------------------------
// Fixture staging guard.
// ---------------------------------------------------------------------------

#[test]
fn fixture_class_file_is_staged() {
    let java = format!("{}/cratonvm/Wp21ClassModern.java", test_resources_dir());
    let class = format!("{}/cratonvm/Wp21ClassModern.class", test_resources_dir());
    assert!(
        std::path::Path::new(&java).exists(),
        "Wp21ClassModern.java fixture must exist at {java}"
    );
    if !std::path::Path::new(&class).exists() {
        eprintln!(
            "WARN: {class} not staged — javac missing or pre-Java-21 \
             toolchain. Functional probes will skip; registry-gate tests \
             still run."
        );
    }
}
