// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.1 — `java.lang.reflect` modern JDK 25 reflection coverage.
//!
//! Verifies the registry-side surface for the WP2.1 net-new natives
//! (trySetAccessible, canAccess, getEnclosingClass typed wrappers, isVarArgs,
//! isBridge, isSynthetic, isDefault, getParameters, etc.) and pins the staging
//! contract for the `apps/reflect_probe/` Java fixture.
//!
//! End-to-end execution of the probe (12 assertions) goes through
//! `apps/reflect_probe/ReflectProbe.java` driven by the `cratonvm` CLI binary.

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_vm::native::register_builtins;

#[test]
fn wp2_1_natives_register_without_panic() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(r.len() > 100, "essential natives < 100, got {}", r.len());
}

#[test]
fn jdk_internal_reflection_are_nest_mates_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "jdk/internal/reflect/Reflection",
            "areNestMates",
            "(Ljava/lang/Class;Ljava/lang/Class;)Z",
        )
        .is_some(),
        "Reflection.areNestMates(Class, Class) must be registered in the real-JDK essential registry"
    );
}

#[test]
fn try_set_accessible_registered_on_method_field_constructor_and_accessibleobject() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    for klass in &[
        "java/lang/reflect/Method",
        "java/lang/reflect/Field",
        "java/lang/reflect/Constructor",
        "java/lang/reflect/AccessibleObject",
    ] {
        assert!(
            r.find(klass, "trySetAccessible", "()Z").is_some(),
            "trySetAccessible must be registered on {klass}"
        );
    }
}

#[test]
fn can_access_registered_on_method_field_constructor() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    for klass in &[
        "java/lang/reflect/Method",
        "java/lang/reflect/Field",
        "java/lang/reflect/Constructor",
    ] {
        assert!(
            r.find(klass, "canAccess", "(Ljava/lang/Object;)Z")
                .is_some(),
            "canAccess must be registered on {klass}"
        );
    }
}

#[test]
fn class_get_enclosing_class_typed_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getEnclosingClass",
            "()Ljava/lang/Class;"
        )
        .is_some(),
        "Class.getEnclosingClass must be registered"
    );
}

#[test]
fn class_get_enclosing_method_and_constructor_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getEnclosingMethod",
            "()Ljava/lang/reflect/Method;"
        )
        .is_some(),
        "getEnclosingMethod must be registered"
    );
    assert!(
        r.find(
            "java/lang/Class",
            "getEnclosingConstructor",
            "()Ljava/lang/reflect/Constructor;"
        )
        .is_some(),
        "getEnclosingConstructor must be registered"
    );
}

#[test]
fn parameter_is_implicit_and_synthetic_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find("java/lang/reflect/Parameter", "isImplicit", "()Z")
            .is_some(),
        "Parameter.isImplicit must be registered"
    );
    assert!(
        r.find("java/lang/reflect/Parameter", "isSynthetic", "()Z")
            .is_some(),
        "Parameter.isSynthetic must be registered"
    );
}

#[test]
fn method_is_var_args_bridge_synthetic_default_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let m = "java/lang/reflect/Method";
    for fn_name in &["isVarArgs", "isBridge", "isSynthetic", "isDefault"] {
        assert!(
            r.find(m, fn_name, "()Z").is_some(),
            "Method.{fn_name} must be registered"
        );
    }
}

#[test]
fn class_get_record_components_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getRecordComponents0",
            "()[Ljava/lang/reflect/RecordComponent;"
        )
        .is_some()
            || r.find(
                "java/lang/Class",
                "getRecordComponents",
                "()[Ljava/lang/reflect/RecordComponent;"
            )
            .is_some(),
        "Class.getRecordComponents must be registered (with or without trailing 0)"
    );
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn record_component_natives_register() {
    // RecordComponent layout (lang_misc::register_p60_record): slot 0=name, 1=type, 2=declaringRecord
    let mut r = NativeMethodRegistry::new();
    register_builtins(&mut r);
    let rc = "java/lang/reflect/RecordComponent";
    assert!(
        r.find(rc, "getName", "()Ljava/lang/String;").is_some(),
        "RecordComponent.getName must be registered"
    );
    assert!(
        r.find(rc, "getType", "()Ljava/lang/Class;").is_some(),
        "RecordComponent.getType must be registered"
    );
    assert!(
        r.find(rc, "getDeclaringRecord", "()Ljava/lang/Class;")
            .is_some(),
        "RecordComponent.getDeclaringRecord must be registered"
    );
}

#[test]
fn class_get_permitted_subclasses_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/Class",
            "getPermittedSubclasses0",
            "()[Ljava/lang/Class;"
        )
        .is_some(),
        "Class.getPermittedSubclasses0 must be registered"
    );
}

#[test]
fn class_is_record_and_is_sealed_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find("java/lang/Class", "isRecord0", "()Z").is_some()
            || r.find("java/lang/Class", "isRecord", "()Z").is_some(),
        "Class.isRecord must be registered (with or without trailing 0)"
    );
    // Class.isSealed is implemented in pure Java in JDK 25 (calls getPermittedSubclasses0)
    // so a missing native is acceptable.
}

#[test]
fn method_get_parameters_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/reflect/Method",
            "getParameters",
            "()[Ljava/lang/reflect/Parameter;"
        )
        .is_some(),
        "Method.getParameters must be registered"
    );
    assert!(
        r.find(
            "java/lang/reflect/Constructor",
            "getParameters",
            "()[Ljava/lang/reflect/Parameter;"
        )
        .is_some(),
        "Constructor.getParameters must be registered"
    );
    assert!(
        r.find(
            "java/lang/reflect/Executable",
            "getParameters",
            "()[Ljava/lang/reflect/Parameter;"
        )
        .is_some(),
        "Executable.getParameters must be registered"
    );
}

#[test]
fn method_get_default_value_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/reflect/Method",
            "getDefaultValue",
            "()Ljava/lang/Object;"
        )
        .is_some(),
        "Method.getDefaultValue must be registered"
    );
}

#[test]
fn field_is_synthetic_and_enum_constant_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let f = "java/lang/reflect/Field";
    assert!(
        r.find(f, "isSynthetic", "()Z").is_some(),
        "Field.isSynthetic must be registered"
    );
    assert!(
        r.find(f, "isEnumConstant", "()Z").is_some(),
        "Field.isEnumConstant must be registered"
    );
}

#[test]
fn constructor_is_synthetic_var_args_get_name_register() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let c = "java/lang/reflect/Constructor";
    assert!(
        r.find(c, "isSynthetic", "()Z").is_some(),
        "Constructor.isSynthetic must be registered"
    );
    assert!(
        r.find(c, "isVarArgs", "()Z").is_some(),
        "Constructor.isVarArgs must be registered"
    );
    assert!(
        r.find(c, "getName", "()Ljava/lang/String;").is_some(),
        "Constructor.getName must be registered"
    );
}

mod common;

#[test]
fn reflect_probe_class_files_exist_when_staged() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let probe = manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("reflect_probe")
        .join("classes");
    if !probe.exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`. `apps/` is gitignored (.gitignore line 12),
        // so this fixture was never tracked and is absent from the tree.
        let _ = common::require_fixture(
            "wp2_1_reflect",
            "the WP2.1 fixture directory `reflect_probe/classes` (built from \
             ReflectProbe.java; this test pins its sealed-permits inner classes)",
            &[probe.clone()],
        );
        return;
    }
    let main_class = probe.join("ReflectProbe.class");
    assert!(main_class.exists(), "ReflectProbe.class must be staged");
    // Sealed-permits classes:
    for inner in &[
        "ReflectProbe$A.class",
        "ReflectProbe$B.class",
        "ReflectProbe$I.class",
        "ReflectProbe$Foo.class",
        "ReflectProbe$MyAnno.class",
    ] {
        assert!(probe.join(inner).exists(), "{inner} must be staged");
    }
}
