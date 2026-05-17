//! Build script for craton-gpu.
//!
//! Compiles the Java annotation source files under `src/main/java/`
//! using `javac` (if available) and packages them into a jar via
//! `jar` (if available). The resulting paths are surfaced to the
//! Rust crate via two `cargo:rustc-env=` variables:
//!
//! * `CRATON_GPU_ANNOTATIONS_JAR` — absolute path to the produced
//!   jar, or empty string when the jar could not be produced.
//! * `CRATON_GPU_ANNOTATIONS_DIR` — absolute path to a directory
//!   that either contains the compiled `.class` files or is empty
//!   (the directory is always created so `env!()` in lib.rs has a
//!   valid value).
//!
//! The build script is intentionally resilient: a missing `javac`,
//! a missing `jar`, or a complete absence of `.java` sources only
//! produces a `cargo:warning=`. It never fails the build.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Always rerun when the Java sources change.
    println!("cargo:rerun-if-changed=src/main/java");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"),
    );
    let classes_dir = out_dir.join("classes");
    let jar_path = out_dir.join("craton-gpu-annotations.jar");

    // Always (re)create the classes dir so the env! in lib.rs has a
    // valid path even when nothing got compiled.
    if let Err(e) = fs::create_dir_all(&classes_dir) {
        println!(
            "cargo:warning=craton-gpu: failed to create {}: {}",
            classes_dir.display(),
            e
        );
    }

    let java_root = PathBuf::from("src/main/java");

    // Helper: emit the env vars and return early.
    let emit_env = |jar: &str, dir: &Path| {
        println!("cargo:rustc-env=CRATON_GPU_ANNOTATIONS_JAR={}", jar);
        println!(
            "cargo:rustc-env=CRATON_GPU_ANNOTATIONS_DIR={}",
            dir.display()
        );
    };

    // 1. Is javac on PATH?
    if !javac_available() {
        println!(
            "cargo:warning=javac not found; craton-gpu annotations will not be compiled"
        );
        emit_env("", &classes_dir);
        return;
    }

    // 2. Collect .java sources.
    let mut sources: Vec<PathBuf> = Vec::new();
    if java_root.is_dir() {
        if let Err(e) = collect_java(&java_root, &mut sources) {
            println!(
                "cargo:warning=craton-gpu: walking {} failed: {}",
                java_root.display(),
                e
            );
        }
    }

    if sources.is_empty() {
        println!(
            "cargo:warning=javac not found; craton-gpu annotations will not be compiled"
        );
        emit_env("", &classes_dir);
        return;
    }

    // 3. Compile with javac.
    let mut javac = Command::new("javac");
    javac.arg("-d").arg(&classes_dir);
    for src in &sources {
        javac.arg(src);
    }
    let javac_status = javac.status();
    match javac_status {
        Ok(status) if status.success() => { /* fall through to jar */ }
        Ok(status) => {
            println!(
                "cargo:warning=craton-gpu: javac exited with {}; annotations not packaged",
                status
            );
            emit_env("", &classes_dir);
            return;
        }
        Err(e) => {
            println!("cargo:warning=craton-gpu: failed to invoke javac: {}", e);
            emit_env("", &classes_dir);
            return;
        }
    }

    // 4. Package with jar (optional).
    let jar_str = match build_jar(&classes_dir, &jar_path) {
        Ok(()) => jar_path.display().to_string(),
        Err(e) => {
            println!(
                "cargo:warning=craton-gpu: jar packaging skipped: {} (classes dir is the fallback)",
                e
            );
            String::new()
        }
    };

    emit_env(&jar_str, &classes_dir);
}

/// Probe for `javac` on PATH by running `javac -version`. Both
/// stdout and stderr are discarded; only the exit status matters.
fn javac_available() -> bool {
    let mut cmd = Command::new("javac");
    cmd.arg("-version");
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    matches!(cmd.status(), Ok(s) if s.success())
}

/// Recursively collect `*.java` files under `root` into `out`.
fn collect_java(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            collect_java(&path, out)?;
        } else if ftype.is_file() {
            if path.extension().map(|e| e == "java").unwrap_or(false) {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// Invoke `jar --create --file <jar> -C <classes_dir> .`.
fn build_jar(classes_dir: &Path, jar_path: &Path) -> Result<(), String> {
    let mut cmd = Command::new("jar");
    cmd.arg("--create");
    cmd.arg("--file");
    cmd.arg(OsString::from(jar_path));
    cmd.arg("-C");
    cmd.arg(OsString::from(classes_dir));
    cmd.arg(".");
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("jar exited with {}", status)),
        Err(e) => Err(format!("failed to invoke jar: {}", e)),
    }
}
