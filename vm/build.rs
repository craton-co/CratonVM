// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Build script for cratonvm-vm.
//!
//! Automatically compiles Java test classes in `tests/resources/cratonvm/` if
//! `javac` is available on the PATH. Generated classes are staged under
//! `OUT_DIR/test-classes` and exposed through `CRATONVM_TEST_CLASSES_DIR`;
//! the build script does not mutate the source tree.
//!
//! Java files are compiled in two passes:
//! 1. Legacy files: plain `javac` (no version flags).
//! 2. Modern files (Java 21+ features like pattern matching, records, sealed
//!    classes): `javac --release 21 --enable-preview`.
//! A file is considered "modern" if its first 20 lines contain the marker
//! comment `// JAVA21+`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Return `true` if the file's header contains `// JAVA21+`.
fn is_modern_java(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().take(20).any(|l| l.contains("// JAVA21+"))
}

fn compile_files(
    files: &[PathBuf],
    out_dir: &Path,
    extra_args: &[&str],
    log_failure: bool,
) -> bool {
    if files.is_empty() {
        return true;
    }
    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(out_dir);
    for arg in extra_args {
        cmd.arg(arg);
    }
    for f in files {
        cmd.arg(f);
    }
    let output = match cmd.output() {
        Ok(output) => output,
        Err(e) => {
            if log_failure {
                println!("cargo:warning=failed to execute javac ({extra_args:?}): {e}");
            }
            return false;
        }
    };
    if !output.status.success() {
        // Only surface the failure as a cargo warning when it's the
        // *final* attempt — speculative "try -source 7 first, fall
        // back on fail" passes must stay silent so `cargo doc` does
        // not pick them up as noise.
        if log_failure {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("cargo:warning=javac failed ({extra_args:?}):\n{stderr}");
        }
        return false;
    }
    true
}

/// Round-11 cross-cutting HIGH-6 (round-9 MED-10): cache the result of
/// the `javac -version` availability probe in `OUT_DIR`. Without the
/// cache this `Command::new("javac")` fork/exec runs on EVERY
/// incremental build of the `cratonvm-vm` crate — even when no .java
/// source changed. On Windows the `CreateProcess` + JVM startup cost
/// alone is ~200ms; on macOS the toolchain shim adds another ~100ms.
///
/// Cache strategy: write `present` or `absent` plus the PATH/JAVA_HOME
/// fingerprint to `$OUT_DIR/javac-version.txt` after the first probe.
/// Subsequent builds with the same environment read the cache and skip
/// the shell-out. The cache is invalidated automatically when:
///   * `OUT_DIR` is wiped (`cargo clean`).
///   * The user updates their `PATH` and triggers a rerun via the
///     `cargo:rerun-if-env-changed=PATH` directive we emit below.
///   * The Java sources change (existing `cargo:rerun-if-changed`).
fn javac_available_cached(out_dir: &Path) -> bool {
    let cache_path = out_dir.join("javac-version.txt");
    let fingerprint = javac_env_fingerprint();
    if let Ok(prev) = std::fs::read_to_string(&cache_path) {
        let mut lines = prev.lines();
        let state = lines.next().unwrap_or("");
        let cached_fingerprint = lines.collect::<Vec<_>>().join("\n");
        if cached_fingerprint == fingerprint {
            return state == "present";
        }
    }
    let javac_check = Command::new("javac").arg("-version").output();
    let present = javac_check.is_ok_and(|o| o.status.success());
    // Best-effort cache write; if the FS is read-only we'll just
    // shell out again next build (no correctness impact).
    let _ = std::fs::write(
        &cache_path,
        format!(
            "{}\n{}\n",
            if present { "present" } else { "absent" },
            fingerprint
        ),
    );
    present
}

fn javac_env_fingerprint() -> String {
    let path = std::env::var_os("PATH")
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    let java_home = std::env::var_os("JAVA_HOME")
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("PATH={path}\nJAVA_HOME={java_home}")
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir_var = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let out_dir = Path::new(&out_dir_var);
    // Java files live under `tests/resources/cratonvm/`. Generated
    // classes are staged under OUT_DIR so builds/tests are read-only
    // with respect to the source checkout. Tests that need freshly
    // compiled fixtures should prefer CRATONVM_TEST_CLASSES_DIR, while
    // legacy tests can still read committed fixtures from tests/resources.
    let sources_dir = Path::new(&manifest_dir).join("tests/resources/cratonvm");
    let output_dir = out_dir.join("test-classes");
    println!(
        "cargo:rustc-env=CRATONVM_TEST_CLASSES_DIR={}",
        output_dir.display()
    );

    // Round-11 cross-cutting HIGH-6: declare an explicit allow-list of
    // rerun triggers so cargo does not re-execute this build script on
    // every unrelated change in the crate.
    //   * .java sources: existing trigger (preserved).
    //   * build.rs itself: cargo emits this implicitly, but listing it
    //     keeps the contract obvious.
    //   * PATH env var: if the user adds/removes a JDK from PATH, the
    //     `javac_available_cached` result must be re-probed.
    println!("cargo:rerun-if-changed=tests/resources/cratonvm/");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=JAVA_HOME");

    // Collect all .java files. Every plain (non-`// JAVA21+`) source here —
    // including `IntrinsicDiff.java` and `SyntheticDiff.java` (the differential
    // exercise programs for the intrinsic table and the synthetic native
    // overlay respectively) — is picked up by this glob and compiled by the
    // legacy pass below, landing at `$OUT_DIR/test-classes/cratonvm/<Name>.class`.
    let java_files: Vec<PathBuf> = std::fs::read_dir(&sources_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "java"))
        .map(|e| e.path())
        .collect();

    if java_files.is_empty() {
        return;
    }

    // Round-11 cross-cutting HIGH-6: cached javac availability probe.
    // The first incremental build runs `javac -version` once; every
    // subsequent build reads $OUT_DIR/javac-version.txt and avoids the
    // 100-200 ms fork/exec.
    let javac_available = javac_available_cached(out_dir);

    if !javac_available {
        println!(
            "cargo:warning=javac not found on PATH — skipping Java test class compilation. \
             Integration tests will be skipped."
        );
        return;
    }

    if let Err(e) = std::fs::remove_dir_all(&output_dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            println!(
                "cargo:warning=failed to clear {} before javac staging: {e}",
                output_dir.display()
            );
            return;
        }
    }
    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        println!(
            "cargo:warning=failed to create javac staging dir {}: {e}",
            output_dir.display()
        );
        return;
    }

    // Partition into legacy and modern files.
    let (modern, legacy): (Vec<_>, Vec<_>) =
        java_files.into_iter().partition(|f| is_modern_java(f));

    // Pass 1: legacy files (try -source 7, fall back to no flags).
    // Only the *fallback* attempt logs on failure — the first attempt
    // is speculative and a mismatched toolchain is expected.
    if !legacy.is_empty()
        && !compile_files(
            &legacy,
            &output_dir,
            &["-source", "7", "-target", "7"],
            false,
        )
    {
        compile_files(&legacy, &output_dir, &[], true);
    }

    // Pass 2: modern files (Java 21+ features: pattern matching, records, sealed classes).
    // Also speculative — if the host javac doesn't support --release 21
    // we fall back to plain javac which handles modern syntax on JDK
    // 21+ toolchains.
    if !modern.is_empty() && !compile_files(&modern, &output_dir, &["--release", "21"], false) {
        compile_files(&modern, &output_dir, &[], true);
    }
}
