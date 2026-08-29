// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.3 — `ClassManager::define_class_with_options` backend conformance.
//!
//! These tests pin down the contract that ALL four `defineClass` entry
//! points (sun.misc.Unsafe.defineClass, jdk.internal.misc.Unsafe.defineClass,
//! MethodHandles.Lookup.defineClass, ClassLoader.defineClass1/2) share:
//!
//!   * Bytes too short → ClassFormatError
//!   * Bad magic       → ClassFormatError
//!   * Name mismatch   → NoClassDefFoundError
//!   * Duplicate define (no allow_redefine) → IncompatibleClassChangeError
//!   * Hidden flag honored (override_name + hidden bypass dup-check)
//!   * allow_redefine replaces existing class in-place
//!   * Code source override populates Class.code_source
//!   * Nest-host attribution writes the supplied class name
//!
//! Fixture: `apps/hello/Hello.class` — a compiled `Hello` class that
//! exists at a stable path under apps/. We use those bytes directly
//! (they're already known to parse).

use cratonvm_classloading::{ClassLoaderId, ClassManager, DefineClassOptions};
use std::path::PathBuf;

/// Load the compiled `Hello` fixture's raw bytes.
fn load_hello_bytes() -> Option<Vec<u8>> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.push("apps");
    path.push("hello");
    path.push("Hello.class");
    std::fs::read(&path).ok()
}

fn fresh_manager() -> ClassManager {
    // No real boot classpath — for backend-level tests we just want a
    // ClassManager we can register classes against. The only class we
    // load is `Hello`, which extends `java/lang/Object`; the manager's
    // `load_class` falls through to a synthetic Object class on its
    // built-in classpath, which is fine.
    ClassManager::new(&[], &[], &[])
}

#[test]
fn rejects_bytes_too_short() {
    let mut cm = fresh_manager();
    let err = cm
        .define_class_with_options(
            "TooShort",
            &[0xCA, 0xFE], // 2 bytes — less than 8-byte header
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("class file too short"),
        "expected ClassFormatError 'too short', got: {msg}"
    );
}

#[test]
fn rejects_bad_magic() {
    let mut cm = fresh_manager();
    let mut bytes = vec![0u8; 16];
    // First four bytes are NOT CAFEBABE.
    bytes[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let err = cm
        .define_class_with_options(
            "BadMagic",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("bad magic"),
        "expected ClassFormatError 'bad magic', got: {msg}"
    );
}

#[test]
fn rejects_class_name_mismatch() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return, // fixture not available
    };
    let mut cm = fresh_manager();
    let err = cm
        .define_class_with_options(
            "NotHello", // class file says "Hello"
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("NoClassDefFound"),
        "expected NoClassDefFoundError on name mismatch, got: {msg}"
    );
}

#[test]
fn rejects_duplicate_define_default() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    cm.define_class_with_options(
        "Hello",
        &bytes,
        ClassLoaderId::Application,
        DefineClassOptions::default(),
    )
    .expect("first define ok");
    let err = cm
        .define_class_with_options(
            "Hello",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("IncompatibleClassChange"),
        "expected IncompatibleClassChangeError on dup, got: {msg}"
    );
}

#[test]
fn allow_redefine_replaces_existing_class() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let id1 = cm
        .define_class_with_options(
            "Hello",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("first define ok");
    let opts = DefineClassOptions {
        allow_redefine: true,
        ..Default::default()
    };
    let id2 = cm
        .define_class_with_options("Hello", &bytes, ClassLoaderId::Application, opts)
        .expect("redefine ok");
    assert_ne!(id1, id2, "redefine must allocate a fresh ClassId");
    // Looking up by name should resolve to the new id.
    let resolved = cm
        .find_class_by_name_in_loader("Hello", ClassLoaderId::Application)
        .expect("Hello must be findable after redefine");
    assert_eq!(
        resolved, id2,
        "lookup must point at the new id, not the old one"
    );
}

#[test]
fn hidden_class_uses_override_name_and_skips_dup_check() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    // First, define Hello normally so its name is occupied.
    cm.define_class_with_options(
        "Hello",
        &bytes,
        ClassLoaderId::Application,
        DefineClassOptions::default(),
    )
    .expect("first define ok");

    // Now define a hidden class derived from the same bytes, under a
    // mangled name. The hidden flag bypasses the dup-check (the
    // mangled name doesn't collide), and the class is registered as
    // hidden so it's NOT discoverable via find_class_by_name.
    let opts = DefineClassOptions {
        override_name: Some("Hello/0xdeadbeef".to_string()),
        hidden: true,
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options("Hello/0xdeadbeef", &bytes, ClassLoaderId::Application, opts)
        .expect("hidden define ok");
    let cls = cm.class_store.get(cid).expect("class exists");
    assert!(cls.is_hidden(), "hidden flag must be set");
    assert_eq!(&*cls.name, "Hello/0xdeadbeef");
}

#[test]
fn code_source_url_threads_to_class() {
    use cratonvm_classloading::CodeSource;
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let opts = DefineClassOptions {
        code_source: Some(CodeSource::from_url("file:/test/loc.jar")),
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options("Hello", &bytes, ClassLoaderId::Application, opts)
        .expect("define ok");
    let cls = cm.class_store.get(cid).expect("class exists");
    let cs = cls.code_source.as_ref().expect("code_source set");
    assert_eq!(cs.url.as_deref(), Some("file:/test/loc.jar"));
}

#[test]
fn nest_host_class_name_overrides_attribute() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let opts = DefineClassOptions {
        nest_host_class_name: Some("LookupHost".to_string()),
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options("Hello", &bytes, ClassLoaderId::Application, opts)
        .expect("define ok");
    let cls = cm.class_store.get(cid).expect("class exists");
    assert_eq!(cls.nest_host.as_deref(), Some("LookupHost"));
}

#[test]
fn three_paths_dispatch_to_same_backend() {
    // Mirror the spec: the same bytes + name combination defined via
    // three paths (default, hidden, redefine) all parse and produce
    // a `Hello` class with consistent `methods` and `name`.
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    // Path 1: default define.
    let mut cm1 = fresh_manager();
    let id1 = cm1
        .define_class_with_options(
            "Hello",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("path-1 define");
    let cls1 = cm1.class_store.get(id1).unwrap();
    let n1 = cls1.name.to_string();
    let m1 = cls1.methods.len();

    // Path 2: hidden define under a mangled name.
    let mut cm2 = fresh_manager();
    let opts2 = DefineClassOptions {
        override_name: Some("Hello/0x1".to_string()),
        hidden: true,
        ..Default::default()
    };
    let id2 = cm2
        .define_class_with_options("Hello/0x1", &bytes, ClassLoaderId::Application, opts2)
        .expect("path-2 define");
    let cls2 = cm2.class_store.get(id2).unwrap();
    let m2 = cls2.methods.len();

    // Path 3: redefine on top of an initial define.
    let mut cm3 = fresh_manager();
    cm3.define_class_with_options(
        "Hello",
        &bytes,
        ClassLoaderId::Application,
        DefineClassOptions::default(),
    )
    .expect("path-3 first");
    let opts3 = DefineClassOptions {
        allow_redefine: true,
        ..Default::default()
    };
    let id3 = cm3
        .define_class_with_options("Hello", &bytes, ClassLoaderId::Application, opts3)
        .expect("path-3 redefine");
    let cls3 = cm3.class_store.get(id3).unwrap();
    let n3 = cls3.name.to_string();
    let m3 = cls3.methods.len();

    // All three should have the same method count (came from the same
    // bytes) and same un-mangled name where applicable.
    assert_eq!(m1, m2, "method count must be identical across paths 1+2");
    assert_eq!(m1, m3, "method count must be identical across paths 1+3");
    assert_eq!(n1, "Hello");
    assert_eq!(n3, "Hello");
}

// ---------------------------------------------------------------------------
// WP2.3-A — Code source defaulting + skip_verification + hidden mangling
// ---------------------------------------------------------------------------

/// WP2.3-A — when no `code_source` is supplied AND classpath discovery
/// fails (in-memory generated class), `define_class_with_options` MUST
/// synthesize a `CodeSource` with a `file:/runtime-defined/...` URL so
/// `Class.code_source` is *never* null. This prevents the downstream
/// `Class.getProtectionDomain()` -> `pd.getCodeSource()` chain from
/// NPE'ing — the exact failure observed in
/// `apps/cglib_probe/cglib.trace.log` before this fix.
#[test]
fn code_source_defaults_to_runtime_url_when_unsupplied() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    // Default options: no code_source, no nest host, no override.
    // The class is being defined from raw bytes, NOT from the classpath
    // (we passed empty classpaths to fresh_manager), so the classpath
    // fallback also returns None.
    let cid = cm
        .define_class_with_options(
            "Hello",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("define ok");
    let cls = cm.class_store.get(cid).expect("class exists");
    let cs = cls
        .code_source
        .as_ref()
        .expect("code_source MUST be non-null on every defined class");
    let url = cs.url.as_deref().expect("synthetic URL must be set");
    assert!(
        url.starts_with("file:/runtime-defined/"),
        "synthetic URL must start with file:/runtime-defined/, got: {url}",
    );
    assert!(
        url.ends_with(".class"),
        "synthetic URL must end with .class, got: {url}"
    );
}

/// WP2.3-A — caller-supplied CodeSource takes precedence over the
/// synthetic default. This is the path used by
/// `defineClass(name, bytes, off, len, ProtectionDomain pd)` where the
/// PD's CodeSource carries a real URL.
#[test]
fn supplied_code_source_takes_precedence_over_default() {
    use cratonvm_classloading::CodeSource;
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let opts = DefineClassOptions {
        code_source: Some(CodeSource::from_url("file:/opt/app.jar")),
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options("Hello", &bytes, ClassLoaderId::Application, opts)
        .expect("define ok");
    let cls = cm.class_store.get(cid).expect("class exists");
    let cs = cls.code_source.as_ref().expect("code_source set");
    assert_eq!(
        cs.url.as_deref(),
        Some("file:/opt/app.jar"),
        "supplied CodeSource URL must win over the synthetic default",
    );
}

/// WP2.3-A — `options.skip_verification = true` records the per-class
/// flag on the manager's side table so the verifier can later skip
/// Pass-3 bytecode type-checking. Default is false (every class is
/// verified normally).
#[test]
fn skip_verification_flag_persists_per_class() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();

    // Default: not skipped.
    let id1 = cm
        .define_class_with_options(
            "Hello",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("define ok");
    assert!(
        !cm.class_skip_bytecode_verification(id1),
        "default define must NOT skip bytecode verification",
    );

    // skip_verification=true should set the flag.
    let mut cm2 = fresh_manager();
    let opts = DefineClassOptions {
        skip_verification: true,
        ..Default::default()
    };
    let id2 = cm2
        .define_class_with_options("Hello", &bytes, ClassLoaderId::Application, opts)
        .expect("define ok");
    assert!(
        cm2.class_skip_bytecode_verification(id2),
        "skip_verification=true must persist on the per-class table",
    );
}

/// WP2.3-A — hidden classes whose `override_name` collides with an
/// already-loaded class get auto-mangled with a unique counter suffix
/// so two CGLIB / ByteBuddy emissions of the same template each get a
/// distinct identity. This is belt-and-suspenders: callers should
/// pre-mangle, but the manager defends against accidental collisions.
#[test]
fn hidden_class_collision_auto_mangles_name() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();

    // First, occupy the slot `Hello/0xtmpl` with a normal define.
    cm.define_class_with_options(
        "Hello",
        &bytes,
        ClassLoaderId::Application,
        DefineClassOptions {
            override_name: Some("Hello/0xtmpl".to_string()),
            ..Default::default()
        },
    )
    .expect("first define ok");

    // Now define a hidden class with the SAME override_name. The
    // manager must auto-mangle to avoid collision.
    let opts = DefineClassOptions {
        override_name: Some("Hello/0xtmpl".to_string()),
        hidden: true,
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options("Hello/0xtmpl", &bytes, ClassLoaderId::Application, opts)
        .expect("hidden define after collision must succeed (auto-mangled)");
    let cls = cm.class_store.get(cid).expect("class exists");
    assert!(cls.is_hidden(), "hidden flag must be set on the new class");
    let n = cls.name.to_string();
    assert!(
        n.starts_with("Hello/0xtmpl/0x"),
        "hidden class name must be mangled with /0x... suffix on collision, got: {n}"
    );
}

/// WP2.3-A — defining many hidden classes with the SAME template name
/// in sequence: each must get a distinct mangled identity so the class
/// store grows without errors (CGLIB emits N proxies of the same
/// superclass over the lifetime of an app server).
#[test]
fn hidden_class_repeated_mangling_remains_unique() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let mut seen_names = std::collections::HashSet::new();
    for _ in 0..5 {
        let opts = DefineClassOptions {
            override_name: Some("Hello/Template".to_string()),
            hidden: true,
            ..Default::default()
        };
        let cid = cm
            .define_class_with_options("Hello/Template", &bytes, ClassLoaderId::Application, opts)
            .expect("hidden define ok");
        let n = cm.class_store.get(cid).unwrap().name.to_string();
        assert!(
            seen_names.insert(n.clone()),
            "every hidden class must have a unique stored name, but {n} repeated",
        );
    }
    assert_eq!(
        seen_names.len(),
        5,
        "5 distinct hidden class identities must be registered"
    );
}

/// WP2.3-A — defining the same name with a different code_source per
/// hidden emission: each one gets its OWN CodeSource (no aliasing).
#[test]
fn hidden_classes_carry_independent_code_sources() {
    use cratonvm_classloading::CodeSource;
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let opts1 = DefineClassOptions {
        override_name: Some("Hello/TplA".to_string()),
        hidden: true,
        code_source: Some(CodeSource::from_url("file:/A.jar")),
        ..Default::default()
    };
    let id1 = cm
        .define_class_with_options("Hello/TplA", &bytes, ClassLoaderId::Application, opts1)
        .expect("first hidden define ok");
    let opts2 = DefineClassOptions {
        override_name: Some("Hello/TplA".to_string()),
        hidden: true,
        code_source: Some(CodeSource::from_url("file:/B.jar")),
        ..Default::default()
    };
    let id2 = cm
        .define_class_with_options("Hello/TplA", &bytes, ClassLoaderId::Application, opts2)
        .expect("second hidden define ok (auto-mangled)");
    assert_ne!(id1, id2);
    let cs1 = cm
        .class_store
        .get(id1)
        .unwrap()
        .code_source
        .as_ref()
        .unwrap();
    let cs2 = cm
        .class_store
        .get(id2)
        .unwrap()
        .code_source
        .as_ref()
        .unwrap();
    assert_eq!(cs1.url.as_deref(), Some("file:/A.jar"));
    assert_eq!(cs2.url.as_deref(), Some("file:/B.jar"));
}

/// WP2.3-A — exact regression for the `getCodeSource on null` NPE.
/// The previous behavior was: in-memory class -> `Class.code_source =
/// None` -> native `getProtectionDomain0()` returned a null PD ->
/// `pd.getCodeSource()` NPE'd. The fix guarantees `code_source` is
/// non-null with a valid `file:/` URL, so the native materializes a
/// real PD whose `getCodeSource()` does not NPE.
///
/// We can't drive the native ProtectionDomain builder directly from a
/// classloading-level test (it lives in `native-builtins`), but we
/// CAN assert the precondition the native relies on — `code_source !=
/// None` AND `code_source.url` does NOT start with the bootstrap
/// sentinel `class:`.
#[test]
fn no_npe_in_memory_class_has_non_bootstrap_code_source() {
    let bytes = match load_hello_bytes() {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options(
            "Hello",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("define ok");
    let cls = cm.class_store.get(cid).expect("class exists");
    let cs = cls
        .code_source
        .as_ref()
        .expect("code_source MUST be non-null (NPE-precondition)");
    let url = cs
        .url
        .as_ref()
        .expect("CodeSource URL MUST be non-null (NPE-precondition)");
    assert!(
        !url.is_empty(),
        "URL must be non-empty so getProtectionDomain0 does not return null PD"
    );
    assert!(
        !url.starts_with("class:"),
        "URL must not start with `class:` (bootstrap sentinel) so the native does not return null PD",
    );
}

/// BUG-10 — build a minimal-but-valid class file whose `this_class` names a
/// protected platform package (`java/lang/<name>`), extending Object, with
/// no fields/methods/attributes. Used to exercise the H5 prohibited-package
/// guard directly (the `Hello` fixture declares `Hello`, which never trips it).
fn minimal_java_lang_class_bytes(simple_name: &str) -> Vec<u8> {
    let this_name = format!("java/lang/{simple_name}");
    let super_name = "java/lang/Object";
    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // magic
    b.extend_from_slice(&[0x00, 0x00]); // minor
    b.extend_from_slice(&[0x00, 0x34]); // major 52 (Java 8)
    b.extend_from_slice(&[0x00, 0x05]); // constant_pool_count = 5 (#1..#4)
                                        // #1 CONSTANT_Class -> #2
    b.push(7);
    b.extend_from_slice(&[0x00, 0x02]);
    // #2 CONSTANT_Utf8 this_name
    b.push(1);
    b.extend_from_slice(&(this_name.len() as u16).to_be_bytes());
    b.extend_from_slice(this_name.as_bytes());
    // #3 CONSTANT_Class -> #4
    b.push(7);
    b.extend_from_slice(&[0x00, 0x04]);
    // #4 CONSTANT_Utf8 super_name
    b.push(1);
    b.extend_from_slice(&(super_name.len() as u16).to_be_bytes());
    b.extend_from_slice(super_name.as_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // access_flags = ACC_PUBLIC|ACC_SUPER
    b.extend_from_slice(&[0x00, 0x01]); // this_class = #1
    b.extend_from_slice(&[0x00, 0x03]); // super_class = #3
    b.extend_from_slice(&[0x00, 0x00]); // interfaces_count
    b.extend_from_slice(&[0x00, 0x00]); // fields_count
    b.extend_from_slice(&[0x00, 0x00]); // methods_count
    b.extend_from_slice(&[0x00, 0x00]); // attributes_count
    b
}

/// BUG-10 — a non-bootstrap loader must NOT be able to define a class in a
/// protected platform package via the ordinary path (the H5 spoofing guard).
#[test]
fn h5_rejects_prohibited_package_for_ordinary_define() {
    let mut cm = fresh_manager();
    let bytes = minimal_java_lang_class_bytes("Bug10OrdinaryEvil");
    let err = cm
        .define_class_with_options(
            "java/lang/Bug10OrdinaryEvil",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions {
                // skip the bytecode verifier — we only want to reach the H5
                // prohibited-package gate, not exercise full verification.
                skip_verification: true,
                ..Default::default()
            },
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Prohibited package name"),
        "ordinary app-loader define into java.* must be rejected, got: {msg}"
    );
}

/// Build a minimal-but-valid class file with `this_class` = `name`,
/// `super_class` = `java/lang/Object`, no fields, and ONE public static
/// no-arg method `getSecrets()Ljava/util/Set;` whose body is just
/// `aconst_null; areturn` — enough to distinguish "real bytecode" from an
/// empty synthetic stub (which has zero methods).
fn minimal_class_with_static_method(name: &str, method_name: &str) -> Vec<u8> {
    let super_name = "java/lang/Object";
    let descriptor = "()Ljava/util/Set;";
    let code_attr_name = "Code";
    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // magic
    b.extend_from_slice(&[0x00, 0x00]); // minor
    b.extend_from_slice(&[0x00, 0x34]); // major 52 (Java 8)
    b.extend_from_slice(&[0x00, 0x08]); // constant_pool_count = 8 (#1..#7)
                                        // #1 CONSTANT_Class -> #2 (this_class)
    b.push(7);
    b.extend_from_slice(&[0x00, 0x02]);
    // #2 CONSTANT_Utf8 this_name
    b.push(1);
    b.extend_from_slice(&(name.len() as u16).to_be_bytes());
    b.extend_from_slice(name.as_bytes());
    // #3 CONSTANT_Class -> #4 (super_class)
    b.push(7);
    b.extend_from_slice(&[0x00, 0x04]);
    // #4 CONSTANT_Utf8 super_name
    b.push(1);
    b.extend_from_slice(&(super_name.len() as u16).to_be_bytes());
    b.extend_from_slice(super_name.as_bytes());
    // #5 CONSTANT_Utf8 method_name
    b.push(1);
    b.extend_from_slice(&(method_name.len() as u16).to_be_bytes());
    b.extend_from_slice(method_name.as_bytes());
    // #6 CONSTANT_Utf8 descriptor
    b.push(1);
    b.extend_from_slice(&(descriptor.len() as u16).to_be_bytes());
    b.extend_from_slice(descriptor.as_bytes());
    // #7 CONSTANT_Utf8 "Code"
    b.push(1);
    b.extend_from_slice(&(code_attr_name.len() as u16).to_be_bytes());
    b.extend_from_slice(code_attr_name.as_bytes());

    b.extend_from_slice(&[0x00, 0x21]); // access_flags = ACC_PUBLIC|ACC_SUPER
    b.extend_from_slice(&[0x00, 0x01]); // this_class = #1
    b.extend_from_slice(&[0x00, 0x03]); // super_class = #3
    b.extend_from_slice(&[0x00, 0x00]); // interfaces_count
    b.extend_from_slice(&[0x00, 0x00]); // fields_count

    // methods_count = 1
    b.extend_from_slice(&[0x00, 0x01]);
    // method_info: public static getSecrets()Ljava/util/Set;
    b.extend_from_slice(&[0x00, 0x09]); // access_flags = ACC_PUBLIC|ACC_STATIC
    b.extend_from_slice(&[0x00, 0x05]); // name_index = #5
    b.extend_from_slice(&[0x00, 0x06]); // descriptor_index = #6
    b.extend_from_slice(&[0x00, 0x01]); // attributes_count = 1
                                        // Code attribute
    b.extend_from_slice(&[0x00, 0x07]); // attribute_name_index = #7 ("Code")
    let code_bytes: [u8; 2] = [0x01, 0xb0]; // aconst_null; areturn
    let attr_len: u32 = 2 + 2 + 4 + code_bytes.len() as u32 + 2 + 2;
    b.extend_from_slice(&attr_len.to_be_bytes()); // attribute_length
    b.extend_from_slice(&[0x00, 0x01]); // max_stack
    b.extend_from_slice(&[0x00, 0x00]); // max_locals
    b.extend_from_slice(&(code_bytes.len() as u32).to_be_bytes()); // code_length
    b.extend_from_slice(&code_bytes); // code
    b.extend_from_slice(&[0x00, 0x00]); // exception_table_length
    b.extend_from_slice(&[0x00, 0x00]); // attributes_count (of Code)

    b.extend_from_slice(&[0x00, 0x00]); // class attributes_count
    b
}

/// Regression for keycloak-clustering-quarkus-testconfig-cmimpl-getsecrets:
/// a class name under an enterprise-stub-eligible prefix (`io/quarkus/...`)
/// that isn't on the classpath first gets fabricated as an empty synthetic
/// stub (mirrors `ClassLoader.loadClass(implClassName)` probing for a
/// not-yet-generated SmallRye `@ConfigMapping` `$$CMImpl`), and REAL
/// bytecode for that exact name is defined afterwards (mirrors SmallRye's
/// own ASM-generated implementation via `MethodHandles.Lookup.defineClass`).
///
/// Before the fix, `define_class_with_options` minted a brand-new,
/// permanently-shadowed `ClassId` for the real bytecode (because the
/// existing stub lived under `ClassLoaderId::Bootstrap`, a different loader
/// than the real define), so every subsequent by-name lookup kept resolving
/// the empty stub and any call against its (non-existent) real methods blew
/// up with `NoSuchMethodError`. The fix upgrades the existing stub in place.
#[test]
fn defining_real_bytecode_upgrades_existing_enterprise_stub_in_place() {
    let name = "io/quarkus/deployment/dev/testing/FakeCMImpl";
    let mut cm = fresh_manager();

    // Step 1: a failed lookup (mirroring `ClassLoader.loadClass`) fabricates
    // an empty synthetic stub for this name, since it matches
    // `is_enterprise_stub_prefix` and isn't on the (empty) classpath.
    let stub_id = cm.load_class(name).expect("stub fallback must succeed");
    {
        let stub = cm.class_store.get(stub_id).expect("stub registered");
        assert!(
            stub.origin.is_compatibility_stub(),
            "must be fabricated as a stub"
        );
        assert!(
            stub.methods.is_empty(),
            "synthetic stub must have no real methods"
        );
    }

    // Step 2: real bytecode for the SAME name is defined afterwards (as
    // SmallRye's runtime ConfigMapping generator does via
    // `MethodHandles.Lookup.defineClass`).
    let bytes = minimal_class_with_static_method(name, "getSecrets");
    let real_id = cm
        .define_class_with_options(
            name,
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("defining real bytecode over an existing stub must succeed");

    // The stub must be upgraded IN PLACE (same ClassId), not shadowed by a
    // second, unreachable-by-name registration.
    assert_eq!(
        real_id, stub_id,
        "real define must upgrade the existing stub's ClassId, not mint a new shadowed one"
    );

    let real = cm.class_store.get(real_id).expect("class exists");
    assert!(
        !real.origin.is_compatibility_stub(),
        "class must no longer be a synthetic stub after upgrade"
    );
    assert!(
        real.methods.iter().any(|m| &*m.name == "getSecrets"),
        "upgraded class must expose the real getSecrets method"
    );

    // Any subsequent by-name lookup must observe the REAL class, matching
    // what `MethodHandles.Lookup.findStatic(cls, "getSecrets", ...)` needs.
    let relooked = cm.get_loaded_class_id(name).expect("still loaded");
    assert_eq!(relooked, real_id);
    let relooked_class = cm.class_store.get(relooked).unwrap();
    assert!(!relooked_class.origin.is_compatibility_stub());
}

/// BUG-10 — a PRIVILEGED define (`Unsafe.defineClass`) bypasses the H5 guard,
/// matching HotSpot, so ByteBuddy/CGLIB can inject an accessor such as
/// `java.lang.ClassLoader$ByteBuddyAccessor$V1`. Same bytes, same loader, only
/// the `privileged_define` flag differs from the rejected case above.
#[test]
fn h5_allows_prohibited_package_for_privileged_define() {
    let mut cm = fresh_manager();
    let bytes = minimal_java_lang_class_bytes("Bug10PrivilegedAccessor");
    let cid = cm
        .define_class_with_options(
            "java/lang/Bug10PrivilegedAccessor",
            &bytes,
            ClassLoaderId::Application,
            DefineClassOptions {
                skip_verification: true,
                privileged_define: true,
                ..Default::default()
            },
        )
        .expect("privileged Unsafe.defineClass into java.* must be allowed");
    let cls = cm.class_store.get(cid).expect("class exists");
    assert_eq!(&*cls.name, "java/lang/Bug10PrivilegedAccessor");
}
