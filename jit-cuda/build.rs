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

    let java_files: Vec<PathBuf> = match std::fs::read_dir(&sources_dir) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "java"))
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

    let javac_check = Command::new("javac").arg("-version").output();
    if !javac_check.is_ok_and(|o| o.status.success()) {
        println!(
            "cargo:warning=javac not found on PATH — GPU fixtures will not be compiled. \
             jit-cuda analyzer tests will panic with 'failed to read fixture'."
        );
        return;
    }

    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(&sources_dir);
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
