// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(feature = "synthetic-jdk")]
// WP2.1-field anchors synthetic reflection Field overrides. Default real-JDK
// mode does not register those synthetic override tables.

//! WP2.1-field — `java.lang.reflect.Field` surface end-to-end probes.
//!
//! Roadmap reference: `wildfly-ejbca-roadmap.md` §5 (Wave 2 — WP2.1
//! hot list: "`Field.get/set` (volatile aware, final check)").
//!
//! # What this suite anchors
//!
//! Closes the WP2.1 Field-side hot list:
//!   1. `Field.getInt(o)` / `Field.setInt(o,v)` round-trip on primitive
//!      int.
//!   2. Volatile-aware path: `Field.getLong/setLong` on a
//!      `volatile long` field exercises the
//!      `volatile_load_fence`/`volatile_store_fence_*` helpers in
//!      `native-builtins/src/lang_class.rs`. The fences map to
//!      `Acquire`/`Release`/`SeqCst` `std::sync::atomic::fence` calls so
//!      the field read/write enforces the JMM volatile memory model.
//!   3. Final check: setting a non-static `final` field without
//!      `setAccessible(true)` throws `IllegalAccessException`; with
//!      `setAccessible(true)` succeeds. Static-final write throws even
//!      with `setAccessible(true)`.
//!   4. Boxing-aware `Field.get`/`Field.set` round-trip Integer.
//!   5. `getName/getType/getModifiers/getDeclaringClass` round-trip.
//!
//! Layout follows the existing `wp2_1_class_reflect_e2e.rs` /
//! `wp7_2_jdbc_core_types_reachable.rs` pattern:
//!   - cheap registry-pin tests up front (catch a registration regression
//!     even if the fixture is missing).
//!   - Java-fixture probes for each acceptance bullet, hard-asserted.
//!   - fixture-staging guard.
//!
//! The Java fixture lives at `vm/tests/resources/cratonvm/Wp21FieldSurface.java`
//! and is auto-compiled by `vm/build.rs` (which detects `javac` on PATH).

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::native::register_builtins;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE_CLASS: &str = "cratonvm/Wp21FieldSurface";

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_compiled() -> bool {
    let path = format!("{}/cratonvm/Wp21FieldSurface.class", test_resources_dir());
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

/// `Field.get` and `Field.set` must be in the essential native registry.
/// This guards against accidental removal of the registration block in
/// `native-builtins/src/lib.rs` (~line 5260).
#[test]
fn field_get_set_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    // Field natives ship via `register_synthetic_overrides`, mirroring the
    // pattern WP7.2 established (`each_jdbc_core_type_has_registered_natives`).
    // The VM's vm_init.rs uses `register_builtins` (essential + synthetic
    // overrides) under `use_synthetic_jdk = true` (the default), so this
    // matches the production registry shape.
    register_builtins(&mut r);
    assert!(
        r.find(
            "java/lang/reflect/Field",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "WP2.1-field: Field.get(Object) native must be registered"
    );
    assert!(
        r.find(
            "java/lang/reflect/Field",
            "set",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
        )
        .is_some(),
        "WP2.1-field: Field.set(Object,Object) native must be registered"
    );
}

/// Typed accessors for int/long must be registered — the volatile-aware
/// path in `field_get_raw`/`field_set_raw` is reached through these.
#[test]
fn field_typed_accessors_registered() {
    let mut r = NativeMethodRegistry::new();
    // Field natives ship via `register_synthetic_overrides`, mirroring the
    // pattern WP7.2 established (`each_jdbc_core_type_has_registered_natives`).
    // The VM's vm_init.rs uses `register_builtins` (essential + synthetic
    // overrides) under `use_synthetic_jdk = true` (the default), so this
    // matches the production registry shape.
    register_builtins(&mut r);
    for (name, sig) in &[
        ("getInt", "(Ljava/lang/Object;)I"),
        ("setInt", "(Ljava/lang/Object;I)V"),
        ("getLong", "(Ljava/lang/Object;)J"),
        ("setLong", "(Ljava/lang/Object;J)V"),
        ("getBoolean", "(Ljava/lang/Object;)Z"),
        ("setBoolean", "(Ljava/lang/Object;Z)V"),
        ("getFloat", "(Ljava/lang/Object;)F"),
        ("setFloat", "(Ljava/lang/Object;F)V"),
        ("getDouble", "(Ljava/lang/Object;)D"),
        ("setDouble", "(Ljava/lang/Object;D)V"),
        ("getByte", "(Ljava/lang/Object;)B"),
        ("setByte", "(Ljava/lang/Object;B)V"),
        ("getShort", "(Ljava/lang/Object;)S"),
        ("setShort", "(Ljava/lang/Object;S)V"),
        ("getChar", "(Ljava/lang/Object;)C"),
        ("setChar", "(Ljava/lang/Object;C)V"),
    ] {
        assert!(
            r.find("java/lang/reflect/Field", name, sig).is_some(),
            "WP2.1-field: Field.{name}{sig} must be registered"
        );
    }
}

/// Field metadata accessors must be registered — `getModifiers` is the
/// load-bearing input to the volatile-aware fence dispatch.
#[test]
fn field_metadata_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    // Field natives ship via `register_synthetic_overrides`, mirroring the
    // pattern WP7.2 established (`each_jdbc_core_type_has_registered_natives`).
    // The VM's vm_init.rs uses `register_builtins` (essential + synthetic
    // overrides) under `use_synthetic_jdk = true` (the default), so this
    // matches the production registry shape.
    register_builtins(&mut r);
    for (name, sig) in &[
        ("getName", "()Ljava/lang/String;"),
        ("getType", "()Ljava/lang/Class;"),
        ("getModifiers", "()I"),
        ("getDeclaringClass", "()Ljava/lang/Class;"),
        ("setAccessible", "(Z)V"),
    ] {
        assert!(
            r.find("java/lang/reflect/Field", name, sig).is_some(),
            "WP2.1-field: Field.{name}{sig} must be registered"
        );
    }
}

// ---------------------------------------------------------------------------
// Functional probes — Java fixture drives the end-to-end path.
// ---------------------------------------------------------------------------

#[test]
fn int_getter_setter_round_trips() {
    if !fixture_compiled() {
        eprintln!(
            "Skipping int_getter_setter_round_trips: \
             Wp21FieldSurface.class not staged (javac unavailable?)"
        );
        return;
    }
    let r = run_probe("intGetterSetterRoundTrips")
        .expect("WP2.1-field: int round-trip probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field: Field.getInt/setInt must round-trip on primitive int"
    );
}

#[test]
fn plain_long_round_trips_diag() {
    if !fixture_compiled() {
        return;
    }
    let r = run_probe("plainLongGetterSetterRoundTripsDiag")
        .expect("WP2.1-field: plain long diag must invoke");
    assert_eq!(r, 1, "plain long setLong: returned {r}");
}

#[test]
fn volatile_long_round_trips() {
    if !fixture_compiled() {
        eprintln!("Skipping volatile_long_round_trips: fixture not staged");
        return;
    }
    let r = run_probe("volatileLongGetterSetterRoundTrips")
        .expect("WP2.1-field: volatile long probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field: Field.getLong/setLong on a volatile long field must \
         round-trip; -2 = ACC_VOLATILE bit missing on getModifiers, \
         -1 = exception thrown. The volatile-aware path lives in \
         `native-builtins/src/lang_class.rs::volatile_*_fence`."
    );
}

#[test]
fn final_check_enforced() {
    if !fixture_compiled() {
        eprintln!("Skipping final_check_enforced: fixture not staged");
        return;
    }
    let r = run_probe("finalCheckEnforced")
        .expect("WP2.1-field: final-check probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field: final-field write rules must be enforced. \
         Sentinels: -1 non-static final accepted without setAccessible, \
         -2 setAccessible+set didn't stick, \
         -3 static final accepted without setAccessible, \
         -4 static final accepted WITH setAccessible (must always throw \
         per Field.set Javadoc), -5 unexpected exception. \
         The check lives in `native-builtins/src/lang_class.rs::check_final_for_set`."
    );
}

#[test]
fn boxing_round_trips() {
    if !fixture_compiled() {
        eprintln!("Skipping boxing_round_trips: fixture not staged");
        return;
    }
    let r = run_probe("boxingRoundTrips").expect("WP2.1-field: boxing probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field: Field.get on int must return Integer; \
         Field.set on int must accept Integer; \
         set with mismatched type (e.g. String) must throw \
         IllegalArgumentException. \
         Sentinels: -1 not Integer, -2 wrong value, -3 set didn't stick, \
         -4 mismatch accepted (should throw)."
    );
}

#[test]
fn metadata_round_trips() {
    if !fixture_compiled() {
        eprintln!("Skipping metadata_round_trips: fixture not staged");
        return;
    }
    let r =
        run_probe("metadataRoundTrips").expect("WP2.1-field: metadata probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field: getName/getType/getModifiers/getDeclaringClass must \
         round-trip with the expected primitive class + modifier bits."
    );
}

#[test]
fn reference_and_array_round_trips() {
    if !fixture_compiled() {
        eprintln!("Skipping reference_and_array_round_trips: fixture not staged");
        return;
    }
    let r = run_probe("referenceAndArrayRoundTrips")
        .expect("WP2.1-field: reference/array probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field: Field.get/set must round-trip String, int[], and \
         volatile Object reference fields."
    );
}

/// Composite probe — single hard pass/fail for the WP2.1-field acceptance.
#[test]
fn all_field_probes_pass() {
    if !fixture_compiled() {
        eprintln!("Skipping all_field_probes_pass: fixture not staged");
        return;
    }
    let r =
        run_probe("allFieldProbesPass").expect("WP2.1-field: composite probe must invoke cleanly");
    assert_eq!(
        r, 1,
        "WP2.1-field acceptance failed: at least one of the probe methods \
         in Wp21FieldSurface returned non-1. Run individual tests for the \
         specific failure mode."
    );
}

// ---------------------------------------------------------------------------
// Fixture staging guard.
// ---------------------------------------------------------------------------

#[test]
fn fixture_class_file_is_staged() {
    let java = format!("{}/cratonvm/Wp21FieldSurface.java", test_resources_dir());
    let class = format!("{}/cratonvm/Wp21FieldSurface.class", test_resources_dir());
    assert!(
        std::path::Path::new(&java).exists(),
        "Wp21FieldSurface.java fixture must exist at {java}"
    );
    if !std::path::Path::new(&class).exists() {
        eprintln!(
            "WARN: {class} not staged — javac missing at build time. \
             Functional probes will skip; registry-pin tests still run."
        );
    }
}
