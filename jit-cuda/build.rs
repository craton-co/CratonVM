// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compile GPU-offload Java fixtures with `javac` if it's on PATH.
//!
//! Mirrors `vm/build.rs`. Sources live in `../test_classes/gpu/`;
//! `.class` files land next to the sources so `test_support.rs` can
//! find them with a stable relative path.
//!
//! No synthesised bytecode: this script's whole job is to turn real
//! Java sources into real `.class` files. If `javac` is missing the
//! build prints a `cargo:warning=` and the analyzer tests panic when
//! they can't read the fixture — which is the desired loud failure.
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

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let sources_dir = Path::new(&manifest_dir)
        .parent()
        .expect("workspace root is jit-cuda/..")
        .join("test_classes")
        .join("gpu");

    println!("cargo:rerun-if-changed={}", sources_dir.display());

    if !sources_dir.exists() {
        println!(
            "cargo:warning=GPU fixtures directory missing: {} \
             (analyzer tests will panic)",
            sources_dir.display()
        );
        return;
    }

    let javac_available = Command::new("javac")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());

    if !javac_available {
        println!(
            "cargo:warning=javac not found on PATH — GPU fixtures will not be compiled. \
             jit-cuda analyzer tests will panic with 'failed to read fixture'."
        );
        return;
    }

    compile_top_level_fixtures(&sources_dir);
    compile_annotation_fixtures(&sources_dir);
}

/// Compile every `*.java` directly under `test_classes/gpu/` (no package).
///
/// Output `.class` files land alongside the sources via `-d <sources_dir>`.
fn compile_top_level_fixtures(sources_dir: &Path) {
    let java_files: Vec<PathBuf> = match std::fs::read_dir(sources_dir) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && e.path().extension().is_some_and(|ext| ext == "java")
            })
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            println!(
                "cargo:warning=could not read {}: {e}",
                sources_dir.display()
            );
            return;
        }
    };

    if java_files.is_empty() {
        return;
    }

    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(sources_dir);

    // Phase 8 #3 — top-level fixtures may import `craton.gpu.*`
    // (e.g. `BenchmarkExplicit.java` uses GpuExecutor). Add the
    // craton-gpu annotations classpath if `craton-gpu`'s build.rs
    // produced one. Same env-var-fallback rules as
    // `compile_annotation_fixtures`.
    let cp_dir = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR")
        .unwrap_or_default();
    let cp_jar = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR")
        .unwrap_or_default();
    let classpath: Option<String> = match (cp_jar.is_empty(), cp_dir.is_empty()) {
        (false, false) => Some(format!("{cp_jar}{}{cp_dir}", classpath_separator())),
        (false, true) => Some(cp_jar),
        (true, false) => Some(cp_dir),
        (true, true) => None,
    };
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
        println!("cargo:warning=javac failed compiling GPU fixtures:\n{stderr}");
    }
}

/// Compile every `*.java` under `test_classes/gpu/annotations/` with the
/// `craton-gpu` annotation classes on the classpath.
///
/// These sources declare `package gpu.annotations;`, so we point `-d`
/// at `test_classes/` and javac drops `.class` files into
/// `test_classes/gpu/annotations/` automatically. Both Item 7 (positive
/// fixtures) and Item 8 (warmup/negative fixtures) drop their `.java`
/// files into this same directory — no further `build.rs` edits needed.
fn compile_annotation_fixtures(sources_dir: &Path) {
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

    let java_files: Vec<PathBuf> = match std::fs::read_dir(&annotations_dir) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && e.path().extension().is_some_and(|ext| ext == "java")
            })
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

    // Build classpath from craton-gpu's exported `links` metadata.
    // Prefer the jar if present, otherwise the classes directory.
    // Either one alone is enough to resolve `craton.gpu.*` imports.
    // Both come in as cargo-mangled `DEP_<LINKS>_<KEY>` env vars;
    // they're empty strings (not unset) when `craton-gpu`'s build.rs
    // couldn't run javac/jar — hence the explicit `is_empty()` checks.
    let cp_dir = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR")
        .unwrap_or_default();
    let cp_jar = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR")
        .unwrap_or_default();

    let classpath: String = match (cp_jar.is_empty(), cp_dir.is_empty()) {
        (false, false) => format!("{cp_jar}{}{cp_dir}", classpath_separator()),
        (false, true) => cp_jar,
        (true, false) => cp_dir,
        (true, true) => {
            // Graceful fallback: craton-gpu's build.rs handles a missing
            // javac by emitting empty strings, and so do we. Skip rather
            // than fail so `jit-cuda` still builds on hosts without a
            // JDK.
            println!(
                "cargo:warning=craton-gpu annotations not built; skipping annotation fixtures"
            );
            return;
        }
    };

    // Sources declare `package gpu.annotations;` — point -d at the parent
    // of that package root (i.e. `test_classes/`) so .class files land in
    // `test_classes/gpu/annotations/`.
    let dest_dir = sources_dir
        .parent()
        .expect("test_classes/gpu has test_classes/ as parent");

    let mut cmd = Command::new("javac");
    cmd.arg("-cp").arg(&classpath);
    cmd.arg("-d").arg(dest_dir);
    for f in &java_files {
        cmd.arg(f);
    }
    let output = cmd
        .output()
        .expect("failed to invoke javac despite -version probe succeeding");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!(
            "cargo:warning=javac failed compiling GPU annotation fixtures:\n{stderr}"
        );
    }
}

#[cfg(windows)]
fn classpath_separator() -> &'static str {
    ";"
}

#[cfg(not(windows))]
fn classpath_separator() -> &'static str {
    ":"
}
