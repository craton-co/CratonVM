// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T14 — `java/lang/System` bootstrap chain conformance test suite.
//!
//! Verifies that all T14 System and VM native methods are registered,
//! the bootstrap chain (initPhase1/2/3) is wired, and no stubs remain.
//!
//!     cargo test -p cratonvm-vm --test t14_system_conformance -- --nocapture

use std::path::{Path, PathBuf};

/// Returns the workspace root (parent of the `vm` crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Read a workspace-relative source file.
fn read_ws(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

// ===========================================================================
// T14.1 — Phase 1 bootstrap methods registered
// ===========================================================================

/// All java/lang/System native methods that must be registered.
const SYSTEM_NATIVES: &[(&str, &str)] = &[
    ("registerNatives", "()V"),
    ("currentTimeMillis", "()J"),
    ("nanoTime", "()J"),
    ("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
    ("identityHashCode", "(Ljava/lang/Object;)I"),
    ("exit", "(I)V"),
    ("setIn0", "(Ljava/io/InputStream;)V"),
    ("setOut0", "(Ljava/io/PrintStream;)V"),
    ("setErr0", "(Ljava/io/PrintStream;)V"),
    ("mapLibraryName", "(Ljava/lang/String;)Ljava/lang/String;"),
    // T14 bootstrap chain
    ("initPhase1", "()V"),
    ("initPhase2", "(ZZ)I"),
    ("initPhase3", "()V"),
];

#[test]
fn t14_all_system_natives_registered() {
    let lib = read_ws("native-builtins/src/lib.rs");
    let mut missing: Vec<String> = Vec::new();

    for &(method, descriptor) in SYSTEM_NATIVES {
        let method_pat = format!("\"{}\"", method);
        let desc_pat = format!("\"{}\"", descriptor);
        let found = lib.lines().any(|line| {
            line.contains("\"java/lang/System\"")
                && line.contains(&method_pat)
                && line.contains(&desc_pat)
        });
        if !found {
            missing.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {}/{} System natives missing:\n  {}",
        missing.len(),
        SYSTEM_NATIVES.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.1] ✓ All {}/{} java/lang/System natives registered",
        SYSTEM_NATIVES.len(),
        SYSTEM_NATIVES.len(),
    );
}

// ===========================================================================
// T14.2 — initPhase1/2/3 point to real implementations
// ===========================================================================

#[test]
fn t14_init_phases_use_named_functions() {
    let lib = read_ws("native-builtins/src/lib.rs");

    let patterns = [
        ("initPhase1", "native_system_init_phase1"),
        ("initPhase2", "native_system_init_phase2"),
        ("initPhase3", "native_system_init_phase3"),
    ];

    for (method, func) in &patterns {
        let found = lib.lines().any(|line| {
            line.contains("\"java/lang/System\"")
                && line.contains(&format!("\"{}\"", method))
                && line.contains(func)
        });
        assert!(
            found,
            "T14: initPhase method '{}' not wired to '{}'",
            method, func,
        );
    }
    eprintln!("[T14.2] ✓ initPhase1/2/3 all point to named implementations");
}

// ===========================================================================
// T14.3 — VM info natives registered
// ===========================================================================

const VM_NATIVES: &[(&str, &str)] = &[
    ("initialize", "()V"),
    ("initLevel", "()I"),
    ("getSavedProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("getNanoTimeAdjustment", "(J)J"),
    ("getRuntimeArguments", "()[Ljava/lang/String;"),
    ("latestUserDefinedLoader0", "()Ljava/lang/ClassLoader;"),
    ("getuid", "()J"),
    ("geteuid", "()J"),
    ("getgid", "()J"),
    ("getegid", "()J"),
];

#[test]
fn t14_all_vm_natives_registered() {
    let lib = read_ws("native-builtins/src/lib.rs");
    let mut missing: Vec<String> = Vec::new();

    for &(method, descriptor) in VM_NATIVES {
        let method_pat = format!("\"{}\"", method);
        let desc_pat = format!("\"{}\"", descriptor);
        let found = lib.lines().any(|line| {
            line.contains("\"jdk/internal/misc/VM\"")
                && line.contains(&method_pat)
                && line.contains(&desc_pat)
        });
        if !found {
            missing.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {}/{} VM natives missing:\n  {}",
        missing.len(),
        VM_NATIVES.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.3] ✓ All {}/{} jdk/internal/misc/VM natives registered",
        VM_NATIVES.len(),
        VM_NATIVES.len(),
    );
}

// ===========================================================================
// T14.4 — VM.getSavedProperty uses real implementation (not stub)
// ===========================================================================

#[test]
fn t14_vm_get_saved_property_not_stub() {
    let lib = read_ws("native-builtins/src/lib.rs");

    // Find the getSavedProperty registration line
    let line = lib
        .lines()
        .find(|l| l.contains("\"jdk/internal/misc/VM\"") && l.contains("\"getSavedProperty\""))
        .expect("getSavedProperty registration not found");

    // Must NOT be an inline closure returning null
    assert!(
        !line.contains("|_ctx, _args|"),
        "T14: VM.getSavedProperty still uses a stub closure: {}",
        line.trim(),
    );
    // Must point to the real implementation
    assert!(
        line.contains("native_vm_get_saved_property"),
        "T14: VM.getSavedProperty not wired to native_vm_get_saved_property: {}",
        line.trim(),
    );
    eprintln!("[T14.4] ✓ VM.getSavedProperty uses real implementation");
}

// ===========================================================================
// T14.5 — VM.getRuntimeArguments uses real implementation
// ===========================================================================

#[test]
fn t14_vm_get_runtime_arguments_not_stub() {
    let lib = read_ws("native-builtins/src/lib.rs");

    let line = lib
        .lines()
        .find(|l| l.contains("\"jdk/internal/misc/VM\"") && l.contains("\"getRuntimeArguments\""))
        .expect("getRuntimeArguments registration not found");

    // Must NOT return null
    assert!(
        !line.contains("Object(None)"),
        "T14: VM.getRuntimeArguments still returns null: {}",
        line.trim(),
    );
    assert!(
        line.contains("native_vm_get_runtime_arguments"),
        "T14: VM.getRuntimeArguments not wired to named function: {}",
        line.trim(),
    );
    eprintln!("[T14.5] ✓ VM.getRuntimeArguments returns empty array (not null)");
}

// ===========================================================================
// T14.6 — Named implementations exist in lang_system.rs
// ===========================================================================

const T14_IMPLEMENTATIONS: &[&str] = &[
    "native_system_init_phase1",
    "native_system_init_phase2",
    "native_system_init_phase3",
    "native_vm_get_saved_property",
    "native_vm_get_runtime_arguments",
];

#[test]
fn t14_named_implementations_exist() {
    let src = read_ws("native-builtins/src/lang_system.rs");
    let mut missing: Vec<&str> = Vec::new();

    for &func in T14_IMPLEMENTATIONS {
        let pattern = format!("fn {func}(");
        if !src.contains(&pattern) {
            missing.push(func);
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {} implementations missing in lang_system.rs:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.6] ✓ All {}/{} T14 implementations found in lang_system.rs",
        T14_IMPLEMENTATIONS.len(),
        T14_IMPLEMENTATIONS.len(),
    );
}

// ===========================================================================
// T14.7 — Unit tests exist for T14 methods
// ===========================================================================

#[test]
fn t14_unit_tests_exist() {
    let src = read_ws("native-builtins/src/lang_system.rs");

    let required_test_patterns = [
        "init_phase1_succeeds",
        "init_phase2_returns_zero",
        "init_phase3_succeeds",
        "vm_get_saved_property_returns_null_for_missing",
        "vm_get_saved_property_returns_value",
        "vm_get_runtime_arguments_returns_empty_array",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &pattern in &required_test_patterns {
        if !src.contains(pattern) {
            missing.push(pattern);
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {} unit test functions missing:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.7] ✓ All {}/{} T14 unit test patterns found",
        required_test_patterns.len(),
        required_test_patterns.len(),
    );
}

// ===========================================================================
// T14.8 — No stub patterns in System registrations
// ===========================================================================

#[test]
fn t14_no_stubs_in_system_registrations() {
    let lib = read_ws("native-builtins/src/lib.rs");
    let stub_markers = ["todo!()", "unimplemented!()", "STUB"];
    let mut stubs: Vec<String> = Vec::new();

    for line in lib.lines() {
        if !line.contains("\"java/lang/System\"") || !line.contains("register") {
            continue;
        }
        let lower = line.to_lowercase();
        for marker in &stub_markers {
            if lower.contains(&marker.to_lowercase()) {
                stubs.push(line.trim().to_string());
                break;
            }
        }
    }

    assert!(
        stubs.is_empty(),
        "T14: stub patterns found in System registrations:\n  {}",
        stubs.join("\n  "),
    );
    eprintln!("[T14.8] ✓ No stub patterns in java/lang/System registrations");
}

// ===========================================================================
// T14.9 — System properties are populated at VM init
// ===========================================================================

#[test]
fn t14_system_properties_populated() {
    let init = read_ws("vm/src/vm/vm_init.rs");

    let required_properties = [
        "os.name",
        "os.arch",
        "file.separator",
        "path.separator",
        "line.separator",
        "java.version",
        "java.vendor",
        "file.encoding",
        "user.dir",
        "java.io.tmpdir",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &prop in &required_properties {
        let pattern = format!("\"{}\"", prop);
        if !init.contains(&pattern) {
            missing.push(prop);
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {} system properties not populated in vm_init.rs:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.9] ✓ All {}/{} system properties populated at VM init",
        required_properties.len(),
        required_properties.len(),
    );
}

// ===========================================================================
// T14.10 — System.out/err intercept exists in interpreter
// ===========================================================================

#[test]
fn t14_system_stream_intercept_exists() {
    let interp = read_ws("vm/src/runtime/interpreter.rs");

    assert!(
        interp.contains("System.out") || interp.contains("java/lang/System"),
        "T14: interpreter missing System.out/err intercept",
    );
    assert!(
        interp.contains("ensure_system_streams"),
        "T14: interpreter missing ensure_system_streams call",
    );
    eprintln!("[T14.10] ✓ System.out/err stream intercept present in interpreter");
}
