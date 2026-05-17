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
//! invocation with `${CRATON_GPU_ANNOTATIONS_DIR}` on the classpath
//! (emitted by the `craton-gpu` crate's own `build.rs`). If that env
//! var is unset/empty we skip the annotation fixtures with a warning
//! rather than failing the build — this lets `jit-cuda` build in
//! isolation when `craton-gpu` hasn't been built yet.

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
    println!("cargo:rerun-if-env-changed=CRATON_GPU_ANNOTATIONS_DIR");
    println!("cargo:rerun-if-env-changed=CRATON_GPU_ANNOTATIONS_JAR");

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

    // Build classpath from craton-gpu's exported env vars. Prefer the jar
    // if present, otherwise the classes directory. Either one alone is
    // enough to resolve `craton.gpu.*` imports.
    let cp_dir = std::env::var("CRATON_GPU_ANNOTATIONS_DIR").unwrap_or_default();
    let cp_jar = std::env::var("CRATON_GPU_ANNOTATIONS_JAR").unwrap_or_default();

    let classpath: String = match (cp_jar.is_empty(), cp_dir.is_empty()) {
        (false, false) => format!("{cp_jar}{}{cp_dir}", classpath_separator()),
        (false, true) => cp_jar,
        (true, false) => cp_dir,
        (true, true) => {
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
