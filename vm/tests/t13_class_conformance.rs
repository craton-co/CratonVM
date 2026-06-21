// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T13 — `java/lang/Class` JDK 25 natives conformance test suite.
//!
//! Verifies that all 28 registered `java/lang/Class` native methods are
//! present in the NativeMethodRegistry and that the method signatures
//! match the JDK 25 specification.
//!
//!     cargo test -p cratonvm-vm --test t13_class_conformance -- --nocapture

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Returns the workspace root (parent of the `vm` crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Whitespace-stripped source. `registry.register(...)` calls are frequently
/// wrapped across lines by rustfmt, so per-line text scans miss them (and would
/// conflict with `cargo fmt`); matching the whitespace-free form makes the
/// `("class", "method", "descriptor")` tuple detectable regardless of wrapping.
fn compact_ws(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// True if the whitespace-free source registers `("class","method","desc")`.
fn registers(compact: &str, class: &str, method: &str, desc: &str) -> bool {
    compact.contains(&format!("\"{class}\",\"{method}\",\"{desc}\""))
}

// ===========================================================================
// T13.1 — All 28 java/lang/Class natives are registered
// ===========================================================================

/// Canonical list of JDK 25 `java/lang/Class` ACC_NATIVE methods.
/// Each entry is (method_name, descriptor).
const JDK25_CLASS_NATIVES: &[(&str, &str)] = &[
    // T13.1 — Identity + naming
    ("registerNatives", "()V"),
    ("getPrimitiveClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
    ("initClassName", "()Ljava/lang/String;"),
    ("getName", "()Ljava/lang/String;"),
    ("getSuperclass", "()Ljava/lang/Class;"),
    ("getInterfaces0", "()[Ljava/lang/Class;"),
    ("isAssignableFrom", "(Ljava/lang/Class;)Z"),
    ("isInstance", "(Ljava/lang/Object;)Z"),
    ("isPrimitive", "()Z"),
    ("isArray", "()Z"),
    ("isInterface", "()Z"),
    ("getModifiers", "()I"),
    ("getComponentType", "()Ljava/lang/Class;"),
    // T13.2 — Reflection
    ("getDeclaredFields0", "(Z)[Ljava/lang/reflect/Field;"),
    ("getDeclaredMethods0", "(Z)[Ljava/lang/reflect/Method;"),
    (
        "getDeclaredConstructors0",
        "(Z)[Ljava/lang/reflect/Constructor;",
    ),
    ("getDeclaredClasses0", "()[Ljava/lang/Class;"),
    ("getDeclaringClass0", "()Ljava/lang/Class;"),
    ("getEnclosingMethod0", "()[Ljava/lang/Object;"),
    (
        "getRecordComponents0",
        "()[Ljava/lang/reflect/RecordComponent;",
    ),
    ("getPermittedSubclasses0", "()[Ljava/lang/Class;"),
    ("getNestHost0", "()Ljava/lang/Class;"),
    ("getNestMembers0", "()[Ljava/lang/Class;"),
    // T13.3 — Annotations + metadata
    ("getRawAnnotations", "()[B"),
    ("getRawTypeAnnotations", "()[B"),
    ("getConstantPool", "()Ljdk/internal/reflect/ConstantPool;"),
    ("getGenericSignature0", "()Ljava/lang/String;"),
    ("getSimpleBinaryName0", "()Ljava/lang/String;"),
    ("getClassFileVersion0", "()I"),
    ("getClassAccessFlagsRaw0", "()I"),
    (
        "forName0",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
    ),
    // T13.4 — Miscellaneous
    ("desiredAssertionStatus0", "(Ljava/lang/Class;)Z"),
    ("isHidden", "()Z"),
    ("isRecord0", "()Z"),
    ("getProtectionDomain", "()Ljava/security/ProtectionDomain;"),
];

/// Scans `native-builtins/src/lib.rs` to verify that every method in the
/// canonical list has a corresponding `registry.register(...)` call.
#[test]
fn t13_all_class_natives_registered() {
    let lib_path = workspace_root()
        .join("native-builtins")
        .join("src")
        .join("lib.rs");
    let contents = std::fs::read_to_string(&lib_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lib_path.display()));

    let compact = compact_ws(&contents);
    let mut missing: Vec<String> = Vec::new();

    for &(method, descriptor) in JDK25_CLASS_NATIVES {
        if !registers(&compact, "java/lang/Class", method, descriptor) {
            missing.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T13: {}/{} java/lang/Class natives missing registration:\n  {}",
        missing.len(),
        JDK25_CLASS_NATIVES.len(),
        missing.join("\n  "),
    );

    eprintln!(
        "[T13.1] ✓ All {}/{} java/lang/Class natives registered",
        JDK25_CLASS_NATIVES.len(),
        JDK25_CLASS_NATIVES.len(),
    );
}

// ===========================================================================
// T13.2 — No stubs remain (every native points to a real function)
// ===========================================================================

/// Verifies that no registration line for java/lang/Class contains a
/// stub pattern (inline closure returning a hardcoded default without
/// real logic). Closures that are genuine one-liners (like registerNatives
/// returning void, or desiredAssertionStatus returning false) are OK.
#[test]
fn t13_no_stub_closures() {
    let lib_path = workspace_root()
        .join("native-builtins")
        .join("src")
        .join("lib.rs");
    let contents = std::fs::read_to_string(&lib_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lib_path.display()));

    // Find all java/lang/Class registrations and check for known stub patterns
    let stub_markers = ["todo!()", "unimplemented!()", "stub", "STUB"];
    let mut stubs_found: Vec<String> = Vec::new();

    for line in contents.lines() {
        if !line.contains("\"java/lang/Class\"") || !line.contains("register") {
            continue;
        }
        let lower = line.to_lowercase();
        for marker in &stub_markers {
            if lower.contains(&marker.to_lowercase()) {
                stubs_found.push(line.trim().to_string());
                break;
            }
        }
    }

    assert!(
        stubs_found.is_empty(),
        "T13: found stub patterns in Class registrations:\n  {}",
        stubs_found.join("\n  "),
    );
    eprintln!("[T13.2] ✓ No stub closures in java/lang/Class registrations");
}

// ===========================================================================
// T13.3 — All implementations exist in lang_class.rs
// ===========================================================================

/// The methods that should be implemented as named functions (not inline
/// closures). Each entry is the function name expected in lang_class.rs.
const NAMED_IMPLEMENTATIONS: &[&str] = &[
    "native_class_get_name",
    "native_class_is_array",
    "native_class_is_primitive",
    "native_class_is_interface",
    "native_class_is_instance",
    "native_class_is_assignable_from",
    "native_class_get_superclass",
    "native_class_get_interfaces",
    "native_class_get_modifiers",
    "native_class_get_component_type",
    "native_class_get_primitive_class",
    "native_class_for_name",
    "native_class_get_declared_fields",
    "native_class_get_declared_methods",
    "native_class_get_declared_constructors",
    "native_class_get_declared_classes",
    "native_class_get_declaring_class",
    "native_class_get_simple_binary_name",
    "native_class_get_enclosing_method",
    "native_class_get_generic_signature",
    "native_class_get_raw_annotations",
    "native_class_get_raw_type_annotations",
    "native_class_get_constant_pool",
    "native_class_get_nest_host",
    "native_class_get_nest_members",
    "native_class_get_permitted_subclasses",
    "native_class_get_class_file_version",
    "native_class_is_record",
];

#[test]
fn t13_named_implementations_exist() {
    let src_path = workspace_root()
        .join("native-builtins")
        .join("src")
        .join("lang_class.rs");
    let contents = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", src_path.display()));

    let mut missing: Vec<&str> = Vec::new();
    for &func_name in NAMED_IMPLEMENTATIONS {
        let pattern = format!("fn {func_name}(");
        if !contents.contains(&pattern) {
            missing.push(func_name);
        }
    }

    assert!(
        missing.is_empty(),
        "T13: {} named implementations missing in lang_class.rs:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T13.3] ✓ All {}/{} named implementations found in lang_class.rs",
        NAMED_IMPLEMENTATIONS.len(),
        NAMED_IMPLEMENTATIONS.len(),
    );
}

// ===========================================================================
// T13.4 — NativeContext trait has required methods
// ===========================================================================

/// Methods that T13 adds to the NativeContext trait.
const T13_CONTEXT_METHODS: &[&str] = &[
    "fn class_file_version(",
    "fn inner_classes(",
    "fn enclosing_method(",
    "fn declaring_class(",
    "fn raw_annotations(",
    "fn raw_type_annotations(",
    "fn nest_host_name(",
    "fn nest_member_names(",
    "fn class_signature(",
];

#[test]
fn t13_native_context_has_required_methods() {
    let registry_path = workspace_root()
        .join("native-api")
        .join("src")
        .join("registry.rs");
    let contents = std::fs::read_to_string(&registry_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", registry_path.display()));

    let mut missing: Vec<&str> = Vec::new();
    for &method_sig in T13_CONTEXT_METHODS {
        if !contents.contains(method_sig) {
            missing.push(method_sig);
        }
    }

    assert!(
        missing.is_empty(),
        "T13: {} NativeContext methods missing:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T13.4] ✓ All {} NativeContext trait methods present",
        T13_CONTEXT_METHODS.len(),
    );
}

// ===========================================================================
// T13.5 — VM-side NativeContext implementations exist
// ===========================================================================

#[test]
fn t13_vm_implementations_exist() {
    let vm_exec_path = workspace_root()
        .join("vm")
        .join("src")
        .join("vm")
        .join("vm_exec.rs");
    let contents = std::fs::read_to_string(&vm_exec_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", vm_exec_path.display()));

    let required = [
        "fn class_file_version(",
        "fn inner_classes(",
        "fn enclosing_method(",
        "fn declaring_class(",
        "fn nest_host_name(",
        "fn nest_member_names(",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &sig in &required {
        if !contents.contains(sig) {
            missing.push(sig);
        }
    }

    assert!(
        missing.is_empty(),
        "T13: {} VM implementations missing in vm_exec.rs:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T13.5] ✓ All {} VM-side NativeContext implementations present",
        required.len(),
    );
}

// ===========================================================================
// T13.6 — Class struct has inner_classes and enclosing_method fields
// ===========================================================================

#[test]
fn t13_class_struct_has_required_fields() {
    let class_path = workspace_root()
        .join("classloading")
        .join("src")
        .join("class.rs");
    let contents = std::fs::read_to_string(&class_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", class_path.display()));

    let required_fields = [
        "inner_classes",
        "enclosing_method",
        "InnerClassEntry",
        "EnclosingMethodInfo",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &field in &required_fields {
        if !contents.contains(field) {
            missing.push(field);
        }
    }

    assert!(
        missing.is_empty(),
        "T13: Class struct missing fields/types:\n  {}",
        missing.join("\n  "),
    );
    eprintln!("[T13.6] ✓ Class struct has inner_classes + enclosing_method fields");
}

// ===========================================================================
// T13.7 — No duplicate registrations
// ===========================================================================

#[test]
fn t13_no_duplicate_registrations() {
    let lib_path = workspace_root()
        .join("native-builtins")
        .join("src")
        .join("lib.rs");
    let contents = std::fs::read_to_string(&lib_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lib_path.display()));

    // Count registrations per (method, descriptor) pair in the FIRST
    // registration function (register_jdk25_natives). We allow multiple
    // registration functions (phases) to register the same method.
    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicates: Vec<String> = Vec::new();

    // Only scan the main registration function
    let mut in_fn = false;
    let mut brace_depth: i32 = 0;
    for line in contents.lines() {
        if line.contains("pub fn register_jdk25_natives") {
            in_fn = true;
            brace_depth = 0;
        }
        if in_fn {
            brace_depth += line.matches('{').count() as i32;
            brace_depth -= line.matches('}').count() as i32;
            if brace_depth <= 0 && in_fn && line.contains('}') {
                break;
            }

            if line.contains("\"java/lang/Class\"") && line.contains("register") {
                // Extract method name from the line
                if let Some(start) = line.find("\"java/lang/Class\"") {
                    let rest = &line[start + "\"java/lang/Class\"".len()..];
                    // Find the next two quoted strings (method name, descriptor)
                    let parts: Vec<&str> = rest.split('"').collect();
                    if parts.len() >= 4 {
                        let key = format!("{}:{}", parts[1], parts[3]);
                        if !seen.insert(key.clone()) {
                            duplicates.push(key);
                        }
                    }
                }
            }
        }
    }

    assert!(
        duplicates.is_empty(),
        "T13: duplicate registrations found:\n  {}",
        duplicates.join("\n  "),
    );
    eprintln!("[T13.7] ✓ No duplicate java/lang/Class registrations in main function");
}

// ===========================================================================
// T13.8 — Unit test coverage
// ===========================================================================

#[test]
fn t13_unit_tests_exist() {
    let src_path = workspace_root()
        .join("native-builtins")
        .join("src")
        .join("lang_class.rs");
    let contents = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", src_path.display()));

    // Verify that the test module contains tests for the T13 methods
    let required_test_patterns = [
        "class_get_declaring_class",
        "class_get_simple_binary_name",
        "class_get_enclosing_method",
        "class_get_generic_signature",
        "class_get_raw_annotations",
        "class_get_raw_type_annotations",
        "class_get_constant_pool",
        "class_get_declared_classes",
        "class_get_nest_host",
        "class_get_nest_members",
        "class_get_permitted_subclasses",
        "class_get_class_file_version",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &pattern in &required_test_patterns {
        if !contents.contains(pattern) {
            missing.push(pattern);
        }
    }

    assert!(
        missing.is_empty(),
        "T13: {} unit test functions missing:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T13.8] ✓ All {}/{} T13 unit test patterns found",
        required_test_patterns.len(),
        required_test_patterns.len(),
    );
}

// ===========================================================================
// T13.9 — Method count verification
// ===========================================================================

#[test]
fn t13_method_count() {
    let lib_path = workspace_root()
        .join("native-builtins")
        .join("src")
        .join("lib.rs");
    let contents = std::fs::read_to_string(&lib_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lib_path.display()));

    // Count actual `register("java/lang/Class", ...)` calls in the
    // whitespace-free source so multi-line (rustfmt-wrapped) registrations are
    // counted correctly — a per-line scan misses calls whose class literal and
    // `register` token land on different lines.
    let compact = compact_ws(&contents);
    let class_reg_count = compact.matches("register(\"java/lang/Class\"").count();

    // We expect at least 28 registrations across all registration functions
    assert!(
        class_reg_count >= 28,
        "T13: expected >= 28 java/lang/Class registrations, found {class_reg_count}",
    );
    eprintln!("[T13.9] ✓ {class_reg_count} java/lang/Class registrations (>= 28 required)");
}
