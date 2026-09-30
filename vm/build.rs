// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Build script for cratonvm-vm.
//!
//! Automatically compiles Java test classes in `tests/resources/cratonvm/` if
//! a `javac` is available. `$JAVA_HOME/bin/javac` is preferred when
//! `JAVA_HOME` is set and points at a real file, falling back to plain
//! `javac` resolved via PATH otherwise. Generated classes are staged under
//! `OUT_DIR/test-classes` and exposed through `CRATONVM_TEST_CLASSES_DIR`;
//! the build script does not mutate the source tree.
//!
//! Java files are compiled in two passes:
//! 1. Legacy files: plain `javac` (no version flags).
//! 2. Modern files (Java 21+ features like pattern matching, records, sealed
//!    classes): `javac --release 21 --enable-preview`.
//!
//! A file is considered "modern" if its first 20 lines contain the marker
//! comment `// JAVA21+`.
//!
//! A file whose header contains `// NEEDS-CLASSPATH` is a hand-run probe that
//! compiles only against a third-party jar this build has no way to supply.
//! It is skipped. Both passes compile their whole set in ONE `javac`
//! invocation, so a single such file fails the entire pass and leaves every
//! other fixture unstaged — which is not a loud failure, because the tests
//! that read `CRATONVM_TEST_CLASSES_DIR` fall back to "no compiled classes,
//! skip" (see `interpreter_tests::class_files_available`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Return `true` if the file's header contains `// JAVA21+`.
fn is_modern_java(path: &Path) -> bool {
    header_marker(path, "// JAVA21+")
}

/// Return `true` if the file's header contains `// NEEDS-CLASSPATH`, i.e. it
/// cannot be compiled without a jar this build script does not have.
fn needs_external_classpath(path: &Path) -> bool {
    header_marker(path, "// NEEDS-CLASSPATH")
}

fn header_marker(path: &Path, marker: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().take(20).any(|l| l.contains(marker))
}

fn compile_files(
    javac: &Path,
    files: &[PathBuf],
    out_dir: &Path,
    extra_args: &[&str],
    log_failure: bool,
) -> bool {
    if files.is_empty() {
        return true;
    }
    let mut cmd = Command::new(javac);
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
            warn_javac_failure(javac, extra_args, files, &output);
        }
        return false;
    }
    true
}

/// Report a javac failure so the NEXT occurrence names the file.
///
/// One `cargo:warning=` per line, deliberately. The build-script protocol is
/// LINE-oriented: cargo reads `cargo:warning=<rest of line>` and ignores every
/// following line that is not itself a directive. The old code emitted
/// `cargo:warning=javac failed ({extra_args:?}):\n{stderr}`, so cargo printed
/// the header and silently dropped the whole compiler transcript — which read
/// as "javac failed with empty stderr" and hid 17 real errors in one file for
/// as long as nobody ran javac by hand.
fn warn_javac_failure(
    javac: &Path,
    extra_args: &[&str],
    files: &[PathBuf],
    output: &std::process::Output,
) {
    // Cap the transcript: a broken batch can produce hundreds of lines, and
    // cargo prints every warning on every build until it is fixed.
    const MAX_LINES: usize = 40;

    println!(
        "cargo:warning=javac failed (status {}, {} file(s), args {extra_args:?}): {}",
        output.status,
        files.len(),
        javac.display()
    );

    // Name the offending sources up front — javac reports them as
    // `<path>:<line>: error: …`, and that first token is the whole answer most
    // of the time.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut blamed: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains(": error:"))
        .filter_map(|l| l.split(".java:").next())
        .map(str::trim)
        .collect();
    blamed.sort_unstable();
    blamed.dedup();
    for file in blamed {
        println!("cargo:warning=  javac error in: {file}.java");
    }

    // Then the transcript. `stdout` too: an empty stderr does not mean the
    // tool said nothing, and that ambiguity is what made this diagnostic
    // useless the first time.
    for (stream, text) in [
        ("stderr", &stderr),
        ("stdout", &String::from_utf8_lossy(&output.stdout)),
    ] {
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            continue;
        }
        for line in lines.iter().take(MAX_LINES) {
            println!("cargo:warning=  [{stream}] {line}");
        }
        if lines.len() > MAX_LINES {
            println!(
                "cargo:warning=  [{stream}] … {} more line(s) suppressed",
                lines.len() - MAX_LINES
            );
        }
    }
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
fn javac_available_cached(out_dir: &Path, javac: &Path) -> bool {
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
    let javac_check = Command::new(javac).arg("-version").output();
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

/// Resolve which `javac` to invoke. Prefers `$JAVA_HOME/bin/javac` (or
/// `javac.exe` on Windows) when `JAVA_HOME` is set and that file exists —
/// this matters on hosts where JAVA_HOME designates a specific JDK that
/// isn't first on PATH (e.g. multiple JDKs installed side by side). Falls
/// back to plain `javac`, resolved via PATH by `Command`/`std::process`.
fn resolve_javac() -> PathBuf {
    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        let exe_name = if cfg!(windows) { "javac.exe" } else { "javac" };
        let candidate = Path::new(&java_home).join("bin").join(exe_name);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("javac")
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
    let mut java_files: Vec<PathBuf> = std::fs::read_dir(&sources_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "java"))
        .map(|e| e.path())
        .collect();

    // Drop the hand-run probes that need a third-party jar. Each pass compiles
    // its whole set in one javac invocation, so leaving one of these in fails
    // the pass and stages NOTHING — every fixture-reading test then quietly
    // skips instead of failing. See the `// NEEDS-CLASSPATH` marker.
    java_files.retain(|f| !needs_external_classpath(f));

    // Re-declare the trigger PER FILE, not just for the directory above.
    //
    // `cargo:rerun-if-changed=<dir>` does not mean "anything under this
    // directory": cargo stats the directory itself, and on Windows editing a
    // file inside it does not move the directory's own mtime. So an edited
    // `.java` did not re-stage its class here, and the tests that read
    // `CRATONVM_TEST_CLASSES_DIR` went on loading the previous bytecode.
    //
    // That fails in the worst possible direction. A fixture that was EXTENDED
    // fails with `Err(ExceptionThrown(..))` from a method that does not exist —
    // confusing, but visible. A fixture that was CHANGED goes on passing
    // against the version it was meant to replace, silently.
    //
    // The directory line stays — it is what catches a file being ADDED or
    // REMOVED, which no per-file line can.
    //
    // NOTE this does NOT cover the committed `.class` files in
    // `tests/resources/cratonvm/` itself. Several test files boot a VM whose
    // whole classpath is that directory, so they read the checked-in bytecode
    // and never see this staging at all; editing one of those fixtures means
    // regenerating and committing its `.class` by hand.
    for f in &java_files {
        println!("cargo:rerun-if-changed={}", f.display());
    }

    if java_files.is_empty() {
        return;
    }

    let javac_path = resolve_javac();

    // Round-11 cross-cutting HIGH-6: cached javac availability probe.
    // The first incremental build runs `javac -version` once; every
    // subsequent build reads $OUT_DIR/javac-version.txt and avoids the
    // 100-200 ms fork/exec.
    let javac_available = javac_available_cached(out_dir, &javac_path);

    if !javac_available {
        println!(
            "cargo:warning=javac not found ({}) — skipping Java test class compilation. \
             Integration tests will be skipped.",
            javac_path.display()
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
            &javac_path,
            &legacy,
            &output_dir,
            &["-source", "7", "-target", "7"],
            false,
        )
    {
        compile_files(&javac_path, &legacy, &output_dir, &[], true);
    }

    // Pass 2: modern files (Java 21+ features: pattern matching, records, sealed classes).
    // Also speculative — if the host javac doesn't support --release 21
    // we fall back to plain javac which handles modern syntax on JDK
    // 21+ toolchains.
    if !modern.is_empty()
        && !compile_files(
            &javac_path,
            &modern,
            &output_dir,
            &["--release", "21"],
            false,
        )
    {
        compile_files(&javac_path, &modern, &output_dir, &[], true);
    }
}
