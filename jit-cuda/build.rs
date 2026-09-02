// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compile GPU-offload Java fixtures with `javac` if it's on PATH.
//!
//! Mirrors `vm/build.rs`. Sources live in `../test_classes/gpu/`;
//! generated `.class` files land under `OUT_DIR/gpu-fixtures` so stale
//! checked-in classes cannot mask a missing `javac` or failed compile.
//!
//! No synthesised bytecode: this script's whole job is to turn real
//! Java sources into real `.class` files. If `javac` is missing the
//! build prints a `cargo:warning=` and the analyzer tests panic when
//! they can't read the generated fixture - which is the desired loud
//! failure.
//!
//! Subdirectory `test_classes/gpu/annotations/` holds fixtures that
//! import `craton.gpu.*`. They're compiled in a separate javac
//! invocation with the `craton-gpu` annotation classpath added (jar
//! or classes-dir, whichever `craton-gpu`'s build.rs produced). The
//! classpath comes in through cargo's `links` mechanism: this crate
//! depends on `craton-gpu` in Cargo.toml, `craton-gpu/Cargo.toml`
//! declares `links = "craton-gpu-annotations"`, and `craton-gpu`'s
//! build.rs emits `cargo:annotations_dir=...` /
//! `cargo:annotations_jar=...`. Cargo re-exposes those to us as
//! `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR` /
//! `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR`. Plain
//! `cargo:rustc-env=` lines are NOT propagated cross-crate — that's
//! the bug this propagation path fixes. If both env vars are missing
//! or empty (javac absent on the build host, etc.) we skip the
//! annotation fixtures with a warning rather than failing the build.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let sources_dir = Path::new(&manifest_dir)
        .parent()
        .expect("workspace root is jit-cuda/..")
        .join("test_classes")
        .join("gpu");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let fixture_out_dir = out_dir.join("gpu-fixtures");

    println!("cargo:rerun-if-changed={}", sources_dir.display());
    println!(
        "cargo:rustc-env=JIT_CUDA_FIXTURE_DIR={}",
        fixture_out_dir.display()
    );
    // Where the compiled `craton.gpu.*` annotation classes live, when
    // this build found them. Exported so `annotations.rs`'s contract
    // test can read the Java enums this crate parses BY NAME and check
    // that the two still agree — see
    // `rust_enum_names_match_the_java_definitions`. Empty string when
    // the `craton-gpu-java` project was not found, which the test
    // reports as a skip rather than treating as a pass.
    println!(
        "cargo:rustc-env=CRATON_GPU_CLASSES_DIR={}",
        std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR").unwrap_or_default()
    );

    if let Err(e) = prepare_clean_dir(&fixture_out_dir) {
        println!(
            "cargo:warning=failed to reset generated GPU fixture directory {}: {e}. Analyzer tests will panic with 'failed to read fixture'.",
            fixture_out_dir.display()
        );
        return;
    }

    if !sources_dir.exists() {
        println!(
            "cargo:warning=GPU fixture sources missing: {}. No generated fixtures were produced at {}; analyzer tests will panic with 'failed to read fixture'.",
            sources_dir.display(),
            fixture_out_dir.display()
        );
        return;
    }

    let javac_available = Command::new("javac")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());

    if !javac_available {
        println!(
            "cargo:warning=javac not found on PATH; GPU fixtures will not be compiled into {}. jit-cuda analyzer tests will panic with 'failed to read fixture' instead of using stale classes.",
            fixture_out_dir.display()
        );
        return;
    }

    if !compile_top_level_fixtures(&sources_dir, &fixture_out_dir) {
        let _ = prepare_clean_dir(&fixture_out_dir);
        return;
    }
    compile_annotation_fixtures(&sources_dir, &fixture_out_dir);
}

/// Compile every `*.java` directly under `test_classes/gpu/` (no package).
///
/// Output `.class` files land under `OUT_DIR/gpu-fixtures` via
/// `-d <fixture_out_dir>`.
fn compile_top_level_fixtures(sources_dir: &Path, fixture_out_dir: &Path) -> bool {
    let mut java_files: Vec<PathBuf> = match std::fs::read_dir(sources_dir) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file() && e.path().extension().is_some_and(|ext| ext == "java"))
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            println!(
                "cargo:warning=could not read {}: {e}",
                sources_dir.display()
            );
            return false;
        }
    };
    java_files.sort();

    let classpath = craton_gpu_classpath();

    if classpath.is_none() {
        let mut skipped = Vec::new();
        java_files.retain(|path| {
            if source_requires_craton_gpu_classpath(path) {
                skipped.push(path.display().to_string());
                false
            } else {
                true
            }
        });
        if !skipped.is_empty() {
            println!(
                "cargo:warning=skipping GPU fixtures that require craton-gpu Java API classes because craton-gpu exported no annotation/runtime classpath: {}",
                skipped.join(", ")
            );
        }
    }

    if java_files.is_empty() {
        println!(
            "cargo:warning=no compilable top-level GPU fixture Java sources found under {}; analyzer tests will panic with 'failed to read fixture'.",
            sources_dir.display()
        );
        return false;
    }

    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(fixture_out_dir);
    if let Some(cp) = &classpath {
        cmd.arg("-cp").arg(cp);
    }

    for f in &java_files {
        cmd.arg(f);
    }
    let output = cmd
        .output()
        .expect("failed to invoke javac despite -version probe succeeding");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!(
            "cargo:warning=javac failed compiling GPU fixtures into {}; generated fixtures were cleared so tests cannot pass against stale .class files:\n{stderr}",
            fixture_out_dir.display()
        );
        return false;
    }
    true
}

fn source_requires_craton_gpu_classpath(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(source) => source.contains("craton.gpu."),
        Err(_) => false,
    }
}

/// Compile every `*.java` under `test_classes/gpu/annotations/` with the
/// `craton-gpu` annotation classes on the classpath.
///
/// These sources declare `package gpu.annotations;`, so we point `-d`
/// at `OUT_DIR/gpu-fixtures` and javac drops `.class` files into
/// `OUT_DIR/gpu-fixtures/gpu/annotations/` automatically. Both Item 7
/// (positive fixtures) and Item 8 (warmup/negative fixtures) drop their
/// `.java` files into this same directory - no further `build.rs` edits
/// needed.
fn compile_annotation_fixtures(sources_dir: &Path, fixture_out_dir: &Path) {
    let annotations_dir = sources_dir.join("annotations");
    println!("cargo:rerun-if-changed={}", annotations_dir.display());
    // The DEP_* env vars are the cargo-mangled form of the
    // `cargo:annotations_dir=` / `cargo:annotations_jar=` metadata
    // emitted by `craton-gpu`'s build.rs. Mangling rule: uppercase
    // the `links` name and replace `-` with `_`, then prepend `DEP_`
    // and append `_<key>` (also uppercased).
    println!("cargo:rerun-if-env-changed=DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR");
    println!("cargo:rerun-if-env-changed=DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR");

    if !annotations_dir.exists() {
        return;
    }

    let mut java_files: Vec<PathBuf> = match std::fs::read_dir(&annotations_dir) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file() && e.path().extension().is_some_and(|ext| ext == "java"))
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            println!(
                "cargo:warning=could not read {}: {e}",
                annotations_dir.display()
            );
            return;
        }
    };

    if java_files.is_empty() {
        return;
    }

    let Some(classpath) = craton_gpu_classpath() else {
        // Graceful fallback: craton-gpu's build.rs handles a missing
        // Java source checkout by emitting an empty classes directory,
        // and so do we. Skip rather than fail so `jit-cuda` still
        // builds on hosts without the optional Java API checkout.
        println!(
            "cargo:warning=craton-gpu annotations not built; skipping annotation fixtures in {}",
            annotations_dir.display()
        );
        return;
    };
    java_files.sort();

    let mut cmd = Command::new("javac");
    cmd.arg("-cp").arg(&classpath);
    cmd.arg("-d").arg(fixture_out_dir);
    for f in &java_files {
        cmd.arg(f);
    }
    let output = cmd
        .output()
        .expect("failed to invoke javac despite -version probe succeeding");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let annotation_out_dir = fixture_out_dir.join("gpu").join("annotations");
        let _ = prepare_clean_dir(&annotation_out_dir);
        println!(
            "cargo:warning=javac failed compiling GPU annotation fixtures into {}; partial annotation outputs were cleared:\n{stderr}",
            annotation_out_dir.display()
        );
    }
}

fn prepare_clean_dir(dir: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir_all(dir)
}

fn craton_gpu_classpath() -> Option<String> {
    let cp_dir = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR").unwrap_or_default();
    let cp_jar = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR").unwrap_or_default();

    let mut entries = Vec::new();
    if classpath_jar_is_usable(Path::new(&cp_jar)) {
        entries.push(cp_jar);
    }
    if classpath_dir_is_usable(Path::new(&cp_dir)) {
        entries.push(cp_dir);
    }

    if entries.is_empty() {
        None
    } else {
        Some(entries.join(classpath_separator()))
    }
}

fn classpath_jar_is_usable(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.is_file()
        && path.metadata().is_ok_and(|metadata| metadata.len() > 0)
}

fn classpath_dir_is_usable(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.is_dir() && dir_contains_class_file(path)
}

fn dir_contains_class_file(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "class") {
            return true;
        }
        if path.is_dir() && dir_contains_class_file(&path) {
            return true;
        }
    }
    false
}

#[cfg(windows)]
fn classpath_separator() -> &'static str {
    ";"
}

#[cfg(not(windows))]
fn classpath_separator() -> &'static str {
    ":"
}
