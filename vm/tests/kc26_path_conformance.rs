// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! KC26 — Path/FileSystem dispatch conformance tests.
//!
//! Verifies that all Path, FileSystem, and FileSystems native methods needed
//! by Keycloak 26 (Quarkus bootstrap) are registered. The primary blocker
//! was `Path.getFileSystem()` abstract dispatch — these tests ensure
//! complete coverage.
//!
//!     cargo test -p cratonvm-vm --test kc26_path_conformance -- --nocapture

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

fn read_ws(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

// ===========================================================================
// KC26.1 — Path.getFileSystem() registered
// ===========================================================================

#[test]
fn kc26_path_get_file_system_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");
    assert!(
        src.contains("\"getFileSystem\"") && src.contains("()Ljava/nio/file/FileSystem;"),
        "[KC26.1] Path.getFileSystem() must be registered"
    );
    println!("[KC26.1] \u{2713} Path.getFileSystem() registered");
}

// ===========================================================================
// KC26.2 — Path.resolve() variants registered
// ===========================================================================

#[test]
fn kc26_path_resolve_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");
    let has_resolve_string =
        src.contains("\"resolve\"") && src.contains("(Ljava/lang/String;)Ljava/nio/file/Path;");
    let has_resolve_path =
        src.contains("\"resolve\"") && src.contains("(Ljava/nio/file/Path;)Ljava/nio/file/Path;");
    assert!(
        has_resolve_string,
        "[KC26.2a] Path.resolve(String) must be registered"
    );
    assert!(
        has_resolve_path,
        "[KC26.2b] Path.resolve(Path) must be registered"
    );
    println!("[KC26.2] \u{2713} Path.resolve(String) and Path.resolve(Path) registered");
}

// ===========================================================================
// KC26.3 — Path.getParent/getRoot/getFileName registered
// ===========================================================================

#[test]
fn kc26_path_navigation_methods_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");

    let methods = &[
        ("getParent", "()Ljava/nio/file/Path;"),
        ("getRoot", "()Ljava/nio/file/Path;"),
        ("getFileName", "()Ljava/nio/file/Path;"),
    ];

    let mut missing = Vec::new();
    for &(name, desc) in methods {
        if !src.contains(&format!("\"{}\"", name)) || !src.contains(&format!("\"{}\"", desc)) {
            missing.push(format!("{name}{desc}"));
        }
    }

    assert!(
        missing.is_empty(),
        "[KC26.3] Missing Path methods: {}",
        missing.join(", ")
    );
    println!("[KC26.3] \u{2713} Path.getParent/getRoot/getFileName registered");
}

// ===========================================================================
// KC26.4 — Path.toString/equals/hashCode registered
// ===========================================================================

#[test]
fn kc26_path_object_methods_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");

    // toString, equals, hashCode should be registered for path
    let path_section_start = src.find("let path = \"java/nio/file/Path\"").unwrap_or(0);
    let path_section = &src[path_section_start..];

    assert!(
        path_section.contains("\"toString\""),
        "[KC26.4a] Path.toString() must be registered"
    );
    assert!(
        path_section.contains("\"equals\""),
        "[KC26.4b] Path.equals() must be registered"
    );
    assert!(
        path_section.contains("\"hashCode\""),
        "[KC26.4c] Path.hashCode() must be registered"
    );
    println!("[KC26.4] \u{2713} Path.toString/equals/hashCode registered");
}

// ===========================================================================
// KC26.5 — FileSystems.getDefault() registered
// ===========================================================================

#[test]
fn kc26_filesystems_get_default_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");
    assert!(
        src.contains("\"java/nio/file/FileSystems\"") && src.contains("\"getDefault\""),
        "[KC26.5] FileSystems.getDefault() must be registered"
    );
    println!("[KC26.5] \u{2713} FileSystems.getDefault() registered");
}

// ===========================================================================
// KC26.6 — FileSystem basic methods registered
// ===========================================================================

#[test]
fn kc26_filesystem_methods_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");

    let methods = &[
        "getSeparator",
        "getPath",
        "provider",
        "isOpen",
        "isReadOnly",
    ];
    let mut missing = Vec::new();

    for &name in methods {
        if !src.contains(&format!("\"{}\"", name)) {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "[KC26.6] Missing FileSystem methods: {}",
        missing.join(", ")
    );
    println!(
        "[KC26.6] \u{2713} FileSystem.getSeparator/getPath/provider/isOpen/isReadOnly registered"
    );
}

// ===========================================================================
// KC26.7 — Path.resolveSibling and Path.relativize registered
// ===========================================================================

#[test]
fn kc26_path_advanced_navigation_registered() {
    let src = read_ws("native-builtins/src/phases_late/nio_file.rs");

    assert!(
        src.contains("\"resolveSibling\""),
        "[KC26.7a] Path.resolveSibling() must be registered"
    );
    assert!(
        src.contains("\"relativize\""),
        "[KC26.7b] Path.relativize() must be registered"
    );
    println!("[KC26.7] \u{2713} Path.resolveSibling/relativize registered");
}

// ===========================================================================
// KC26.8 — Invokedynamic fallback coverage
// ===========================================================================

#[test]
fn kc26_invokedynamic_makeconcat_and_altmetafactory() {
    let src = read_ws("vm/src/runtime/invokedynamic.rs");

    assert!(
        src.contains("MAKE_CONCAT") && src.contains("\"makeConcat\""),
        "[KC26.8a] StringConcatFactory.makeConcat (no-recipe) must be supported"
    );
    assert!(
        src.contains("ALT_METAFACTORY") && src.contains("\"altMetafactory\""),
        "[KC26.8b] LambdaMetafactory.altMetafactory must be supported"
    );
    assert!(
        // The graceful fallback for an unrecognized bootstrap method is the
        // generic bootstrap path (`bootstrap_generic`) plus the loud
        // BootstrapMethodError fallback (`raise_bootstrap_method_error`). The
        // old probe looked for `fallback_unrecognized_bsm`, an identifier that
        // never existed in the source; assert the real mechanism instead.
        src.contains("bootstrap_generic") && src.contains("raise_bootstrap_method_error"),
        "[KC26.8c] Graceful fallback for unrecognized BSMs must exist"
    );
    println!("[KC26.8] \u{2713} makeConcat + altMetafactory + BSM fallback present");
}

// ===========================================================================
// KC26.9 — <clinit> workaround covers invokedynamic errors
// ===========================================================================

#[test]
fn kc26_clinit_workaround_covers_invokedynamic() {
    let src = read_ws("vm/src/vm/vm_util.rs");

    assert!(
        src.contains("\"invokedynamic\""),
        "[KC26.9] <clinit> workaround must catch invokedynamic errors"
    );
    println!("[KC26.9] \u{2713} <clinit> workaround catches invokedynamic NotImplemented errors");
}
