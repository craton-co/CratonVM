use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn validate_module_name_accepts_valid_names() {
    assert!(validate_module_name("java.base").is_ok());
    assert!(validate_module_name("java.sql").is_ok());
    assert!(validate_module_name("org.example.feature").is_ok());
    assert!(validate_module_name("x").is_ok()); // minimal
}

#[test]
fn validate_module_name_rejects_path_traversal() {
    assert!(validate_module_name("../etc/passwd").is_err());
    assert!(validate_module_name("../../win.ini").is_err());
    assert!(validate_module_name("java..base").is_err());
}

#[test]
fn validate_module_name_rejects_path_separators() {
    assert!(validate_module_name("java/base").is_err());
    assert!(validate_module_name("java\\base").is_err());
    assert!(validate_module_name("C:\\Windows").is_err());
    assert!(validate_module_name("java:base").is_err());
}

#[test]
fn validate_module_name_rejects_control_bytes() {
    assert!(validate_module_name("java.base\0").is_err());
    assert!(validate_module_name("java\nbase").is_err());
    assert!(validate_module_name("java\tbase").is_err());
}

#[test]
fn validate_module_name_rejects_empty_and_oversize() {
    assert!(validate_module_name("").is_err());
    let long = "a".repeat(300);
    assert!(validate_module_name(&long).is_err());
}

#[test]
fn module_layer_boot_returns_non_null_layer() {
    let mut ctx = MockNativeContext::new();
    let result = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
    match result {
        Value::Object(Some(_)) => {}
        other => panic!("expected non-null ModuleLayer, got {:?}", other),
    }
}

#[test]
fn find_module_returns_populated_optional() {
    let mut ctx = MockNativeContext::new();
    let layer_v = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
    let name_str = ctx.create_string("java.base");
    let args = [layer_v, Value::Object(Some(name_str))];
    let result = native_module_layer_find_module(&mut ctx, &args)
        .unwrap()
        .unwrap();
    if let Value::Object(Some(opt)) = result {
        // Optional.value should be non-null (a Module)
        match ctx.get_field(opt, 0) {
            Value::Object(Some(_module)) => {}
            other => panic!("expected Optional(Module), got {:?}", other),
        }
    } else {
        panic!("expected non-null Optional");
    }
}

#[test]
fn find_module_rejects_null_name() {
    let mut ctx = MockNativeContext::new();
    let layer_v = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
    let args = [layer_v, Value::Object(None)];
    let err = native_module_layer_find_module(&mut ctx, &args).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("null") || s.contains("Null"),
        "expected NPE, got {}",
        s
    );
}

#[test]
fn find_module_rejects_malicious_name() {
    let mut ctx = MockNativeContext::new();
    let layer_v = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
    let name_str = ctx.create_string("../../evil");
    let args = [layer_v, Value::Object(Some(name_str))];
    let err = native_module_layer_find_module(&mut ctx, &args).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("traversal") || s.contains("IllegalArgument"),
        "expected IAE, got {}",
        s
    );
}

/// `Module.getPackages()` returns a set in the REAL `HashSet` shape: one
/// field, `map`, holding a populated `HashMap`.
///
/// The layout is declared to the mock so `build_real_layout_string_hashset`
/// takes its real branch, which is the branch a real-JDK run takes and the
/// one `build_package_set`'s doc is about. This assertion used to read
/// `ctx.get_field(set, 1)` and expect an element COUNT there, which is the
/// fabricated `(array, size, capacity)` shape -- on a class whose one real
/// instance field is `map`, so slot 1 is off the end of it. That shape is
/// what `getPackages().size()` answering 0 came from in the first place,
/// and the last unconditional write of it (the helper's fallback arm) went
/// with the synthetic slot floor on 2026-09-12. Asserting it here would
/// have required putting that write back.
#[test]
fn module_get_packages_returns_populated_set_for_java_base() {
    let mut ctx = MockNativeContext::new();
    // The real JDK 25 layouts, so `build_real_layout_string_hashset` takes
    // its REAL branch -- the branch a real-JDK run takes. `declare` below
    // registers the class id the way `ensure_class_initialized` does and
    // then attaches the field metadata `resolve_field_index` reads.
    fn declare(ctx: &mut MockNativeContext, class_name: &str, fields: &[(&str, &str)]) {
        let cid = ctx
            .ensure_class_initialized(class_name)
            .expect("mock class registration cannot fail");
        let metadata = fields
            .iter()
            .enumerate()
            .map(
                |(slot_index, (name, descriptor))| cratonvm_native_api::FieldMetadata {
                    name: (*name).to_string(),
                    descriptor: (*descriptor).to_string(),
                    access_flags: 0,
                    slot_index,
                    declaring_class_id: cid,
                    is_static: false,
                },
            )
            .collect();
        ctx.set_declared_fields(cid, metadata);
    }
    declare(
        &mut ctx,
        "java/util/HashMap",
        &[
            ("table", "[Ljava/util/HashMap$Node;"),
            ("size", "I"),
            ("threshold", "I"),
            ("loadFactor", "F"),
            ("entrySet", "Ljava/util/Set;"),
        ],
    );
    declare(
        &mut ctx,
        "java/util/HashMap$Node",
        &[
            ("hash", "I"),
            ("key", "Ljava/lang/Object;"),
            ("value", "Ljava/lang/Object;"),
            ("next", "Ljava/util/HashMap$Node;"),
        ],
    );
    declare(
        &mut ctx,
        "java/util/HashSet",
        &[("map", "Ljava/util/HashMap;")],
    );

    let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
    let module = build_module(&mut ctx, "java.base", layer)
        .expect("build_module must succeed for a synthetic layer");
    let result = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
        .unwrap()
        .unwrap();
    let Value::Object(Some(set)) = result else {
        panic!("expected non-null Set, got {:?}", result);
    };
    // Slot 0 is `HashSet.map`, the class's ONLY instance field.
    let Value::Object(Some(map)) = ctx.get_field(set, 0) else {
        panic!(
            "HashSet.map must hold the backing HashMap, got {:?}",
            ctx.get_field(set, 0)
        );
    };
    // Slot 1 of the map is `HashMap.size`, and it is where the package
    // count actually lives.
    match ctx.get_field(map, 1) {
        Value::Int(n) => assert!(n > 0, "backing map size must be > 0, got {}", n),
        other => panic!("expected Int for HashMap.size, got {:?}", other),
    }
    // The table is a real bucket array, not the element array the
    // fabricated shape used to put at slot 0 of the SET.
    match ctx.get_field(map, 0) {
        Value::Object(Some(_)) => {}
        other => panic!("expected a bucket array in HashMap.table, got {:?}", other),
    }
}

#[test]
fn module_get_packages_rejects_null_receiver() {
    let mut ctx = MockNativeContext::new();
    let err = native_module_get_packages(&mut ctx, &[Value::Object(None)]).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("null") || s.contains("Null"),
        "expected NPE, got {}",
        s
    );
}

#[test]
fn module_get_packages_lazily_populates_empty_module() {
    let mut ctx = MockNativeContext::new();
    // build_module records an EMPTY package list for any non-java.base
    // name (module_packages_table) — getPackages() must still return a
    // valid (non-null) Set, not null/panic.
    let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
    let module = build_module(&mut ctx, "java.sql", layer)
        .expect("build_module must succeed for a synthetic layer");
    let result = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
        .unwrap()
        .unwrap();
    match result {
        Value::Object(Some(_)) => {}
        other => panic!("expected non-null populated Set, got {:?}", other),
    }
}

#[test]
fn module_get_name_returns_string() {
    let mut ctx = MockNativeContext::new();
    let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
    let module = build_module(&mut ctx, "java.base", layer)
        .expect("build_module must succeed for a synthetic layer");
    let result = native_module_get_name(&mut ctx, &[Value::Object(Some(module))])
        .unwrap()
        .unwrap();
    match result {
        Value::Object(Some(s)) => {
            let name = ctx.read_string(s).unwrap_or_default();
            assert_eq!(name, "java.base");
        }
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn register_jboss_jdkspecific_adds_surface() {
    let mut r = NativeMethodRegistry::new();
    let before = r.len();
    register_jboss_jdkspecific(&mut r);
    let after = r.len();
    // We add boot, findModule, getName, getPackages, getLayer,
    // getClassLoader (T19_H12_) — 6.
    assert!(
        after >= before + 6,
        "expected at least 6 new registrations, got {}",
        after - before
    );
}

// T19_H12_TESTS — Module.getClassLoader native must return null so
// downstream `Class.forName(Module, String)` paths fall through to
// `BootLoader.loadClass` rather than dispatching `loadClass(...)` on
// a stale receiver class (the bug was a HashSet receiver because
// synthetic Module slot 2 holds the packages set).
#[test]
fn t19_h12_module_get_classloader_returns_null() {
    let mut r = NativeMethodRegistry::new();
    register_jboss_jdkspecific(&mut r);
    let cb = r
        .find(
            "java/lang/Module",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )
        .expect("Module.getClassLoader must be registered");
    let mut ctx = MockNativeContext::new();
    let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
    let module = build_module(&mut ctx, "java.base", layer)
        .expect("build_module must succeed for a synthetic layer");
    let result = cb(&mut ctx, &[Value::Object(Some(module))])
        .unwrap()
        .unwrap();
    // Per JDK 25 spec: boot-layer modules with bootstrap loader return null.
    assert!(
        matches!(result, Value::Object(None)),
        "expected null ClassLoader for boot-layer module, got {:?}",
        result
    );
}

#[test]
fn t19_h12_module_get_classloader_returns_null_even_with_recorded_packages() {
    // Defense in depth: getClassLoader must not be affected by (or leak)
    // whatever `build_module` recorded for this module's packages in
    // `module_packages_table` — the historical bug this guarded against
    // was a raw field slot double-booked for both purposes; that's no
    // longer possible now that packages are stored off-object entirely,
    // but keep the regression coverage that getClassLoader is correct
    // for a module with real recorded package data.
    let mut r = NativeMethodRegistry::new();
    register_jboss_jdkspecific(&mut r);
    let cb = r
        .find(
            "java/lang/Module",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )
        .expect("Module.getClassLoader must be registered");
    let mut ctx = MockNativeContext::new();
    let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
    // Build a fully-populated module (java.base records the full JDK package list).
    let module = build_module(&mut ctx, "java.base", layer)
        .expect("build_module must succeed for a synthetic layer");
    // Sanity: getPackages() reflects the recorded data for this module.
    let pkgs = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
        .unwrap()
        .unwrap();
    assert!(
        matches!(pkgs, Value::Object(Some(_))),
        "getPackages() should return a populated Set"
    );
    // getClassLoader must NOT be affected by any of this.
    let result = cb(&mut ctx, &[Value::Object(Some(module))])
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Value::Object(None)),
        "Module.getClassLoader must not leak packages data, got {:?}",
        result
    );
}
