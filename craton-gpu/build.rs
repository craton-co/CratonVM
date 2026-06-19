// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Build script for craton-gpu.
//!
//! Compiles the Java annotation source files using `javac` (if
//! available) and packages them into a jar via `jar` (if available).
//!
//! The `.java` sources are NOT shipped inside this crate — they live in
//! an external standalone Maven project (the craton-gpu-java repo). The
//! source tree is located at build time via, in priority order: the
//! `$CRATON_GPU_JAVA_SRC` env override, a `../craton-gpu-java/...`
//! sibling checkout, or (on Windows only) a `C:/craton/...` default
//! install. See `resolve_java_root` for the exact resolution. When no
//! source tree is found the build degrades gracefully to an empty jar
//! plus a `cargo:warning=` — it never fails.
//!
//! The resulting paths are surfaced to the Rust crate via two
//! `cargo:rustc-env=` variables:
//!
//! * `CRATON_GPU_ANNOTATIONS_JAR` — absolute path to the produced
//!   jar, or empty string when the jar could not be produced.
//! * `CRATON_GPU_ANNOTATIONS_DIR` — absolute path to a directory
//!   that either contains the compiled `.class` files or is empty
//!   (the directory is always created so `env!()` in lib.rs has a
//!   valid value).
//!
//! The same two paths are *also* emitted as cargo build-script
//! metadata via the `links = "craton-gpu-annotations"` declaration
//! in `Cargo.toml`:
//!
//! * `cargo:annotations_dir=...`
//! * `cargo:annotations_jar=...`
//!
//! Cargo exposes these to dependents' build scripts as env vars
//! `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR` /
//! `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR`. The `rustc-env` form
//! alone is not enough — cargo intentionally does NOT propagate
//! `rustc-env=` vars to dependents' build scripts.
//!
//! The build script is intentionally resilient: a missing `javac`,
//! a missing `jar`, or a complete absence of `.java` sources only
//! produces a `cargo:warning=`. It never fails the build.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Java sources moved to a standalone Maven project (the
    // craton-gpu-java repo) — see that repo's README.md. They are NOT
    // shipped inside this crate. Locate them via, in priority order:
    //   1. $CRATON_GPU_JAVA_SRC env var (full absolute path to a
    //      directory containing `craton/gpu/*.java`); a set-but-invalid
    //      value is diagnosed via cargo:warning and then ignored,
    //   2. ../craton-gpu-java/src/main/java (portable sibling checkout;
    //      tried on every platform),
    //   3. C:/craton/craton-gpu-java/src/main/java (Windows-only default
    //      install; never consulted on Linux/macOS).
    // If none exists, the build script emits empty paths and a warning
    // — same fallback behaviour as before the move. The build NEVER fails.
    println!("cargo:rerun-if-env-changed=CRATON_GPU_JAVA_SRC");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
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

    let java_root = resolve_java_root();
    // Tell cargo to rerun when the chosen source tree changes. Only emit
    // this when `java_root` actually exists: `resolve_java_root` returns a
    // default candidate even when nothing is present, and cargo treats a
    // missing `rerun-if-changed` path as perpetually dirty, which would
    // force this build script to re-run on every build on hosts without
    // the Java sources. The early-return paths below still emit the
    // rustc-env / cargo metadata lines unconditionally.
    if java_root.is_dir() {
        println!("cargo:rerun-if-changed={}", java_root.display());
    }

    // Helper: emit ALL four lines (two rustc-env, two cargo metadata)
    // and return. The rustc-env lines feed `env!()` in this crate's
    // own `src/lib.rs`; the `cargo:annotations_*` lines feed
    // `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_*` in dependents'
    // build.rs (via the `links` key in Cargo.toml).
    let emit_env = |jar: &str, dir: &Path| {
        println!("cargo:rustc-env=CRATON_GPU_ANNOTATIONS_JAR={}", jar);
        println!(
            "cargo:rustc-env=CRATON_GPU_ANNOTATIONS_DIR={}",
            dir.display()
        );
        // Build-script metadata for dependents (see Cargo.toml `links`).
        // Empty strings are fine — dependents must tolerate them.
        println!("cargo:annotations_jar={}", jar);
        println!("cargo:annotations_dir={}", dir.display());
    };

    // 1. Is javac on PATH?
    if !javac_available() {
        println!("cargo:warning=javac not found; craton-gpu annotations will not be compiled");
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

    // Re-run when any individual `.java` source changes. The directory
    // `rerun-if-changed` above is not enough: on many platforms a
    // directory's mtime does not change when a file *inside* it is
    // edited, so per-file lines are required to catch edits to
    // existing annotation sources.
    for src in &sources {
        println!("cargo:rerun-if-changed={}", src.display());
    }

    if sources.is_empty() {
        println!(
            "cargo:warning=craton-gpu: no .java source files found; annotations will not be compiled"
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

/// Resolve the Java source root, in priority order:
/// 1. `$CRATON_GPU_JAVA_SRC` (treated as an absolute path to a
///    directory containing `craton/gpu/*.java`). A set-but-invalid
///    value is diagnosed via `cargo:warning=` and then ignored.
/// 2. `../craton-gpu-java/src/main/java` (sibling of CratonVM repo) —
///    portable, tried on every platform.
/// 3. `C:/craton/craton-gpu-java/src/main/java` (default install) —
///    **Windows only**; never consulted on Linux/macOS.
///
/// Returns the first path that exists; if none exists, returns a
/// platform-appropriate fallback (the Windows install path on Windows,
/// the OS-agnostic sibling path elsewhere) — the caller (`main`) will
/// discover the absence and emit a `cargo:warning=` instead of failing
/// the build.
fn resolve_java_root() -> PathBuf {
    if let Some(v) = std::env::var_os("CRATON_GPU_JAVA_SRC") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return p;
        }
        // Fix (1): a set-but-invalid override used to be silently ignored,
        // so a typo'd path would mysteriously fall back to the built-in
        // candidates with no clue why the override "did nothing". Diagnose
        // it via cargo:warning. We still fall through to the candidate list
        // (and ultimately to an empty-jar warning) per the no-fail contract.
        println!(
            "cargo:warning=craton-gpu: $CRATON_GPU_JAVA_SRC is set to {} but that is not a directory; ignoring the override and falling back to the default source-resolution candidates",
            p.display()
        );
    }
    // The first candidate is relative and OS-agnostic: cargo guarantees
    // the build script's CWD is the crate root (craton-gpu/), so `..`
    // resolves to the CratonVM workspace parent and finds a sibling
    // craton-gpu-java checkout. This is the portable fallback used on
    // every platform.
    let sibling = PathBuf::from("../craton-gpu-java/src/main/java");
    if sibling.is_dir() {
        return sibling;
    }
    // Fix (3): the `C:/craton/...` absolute default is Windows-only — on
    // Linux/macOS it can never exist and, worse, returning it as the final
    // fallback used to surface a bogus Windows path in the build warning,
    // making a clean non-Windows checkout look broken. Only consult (and
    // only return) the Windows install path when actually building on
    // Windows. On other hosts the OS-agnostic sibling path is the final
    // fallback; `main` discovers its absence and emits a `cargo:warning=`,
    // degrading gracefully to an empty jar per the resilient-build contract.
    #[cfg(windows)]
    {
        let win_default = PathBuf::from("C:/craton/craton-gpu-java/src/main/java");
        if win_default.is_dir() {
            return win_default;
        }
        return win_default;
    }
    // Non-Windows: return the portable sibling path (which does not exist
    // here, by the check above) so the warning names a sensible relative
    // location and instructs the user to set $CRATON_GPU_JAVA_SRC.
    #[cfg(not(windows))]
    {
        sibling
    }
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
///
/// NOTE (reproducibility): the JDK `jar` tool embeds each entry's file
/// modification timestamp into the archive, so the produced jar is NOT
/// byte-reproducible across builds even when the `.class` inputs are
/// identical. There is no portable `jar` flag to normalize timestamps
/// (the `--date` option only exists on recent JDKs and is not relied on
/// here), so we leave the behaviour as-is. Downstream consumers that
/// cache on content should key on the compiled class directory
/// (`CRATON_GPU_ANNOTATIONS_DIR`), whose contents are deterministic,
/// rather than on the hash of this jar.
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
