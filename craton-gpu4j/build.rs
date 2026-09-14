// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Build script for craton-gpu4j.
//
// Compiles the Java annotation source files using `javac` when available
// and packages them into a jar with `jar` when available.
//
// The `.java` sources are not shipped inside this crate. They live in an
// external standalone Maven project, the gpu4j repo (formerly
// craton-gpu-java). The source tree is located at build time via, in
// priority order: the
// `$CRATON_GPU_JAVA_SRC` env override, a `gpu4j` checkout beside the
// CratonVM workspace, or on Windows only, the `C:/craton/...` default
// install. When no source tree is found the build degrades gracefully to an
// empty annotations directory plus a `cargo:warning=`; it never fails.
//
// The resulting paths are surfaced to the Rust crate via two
// `cargo:rustc-env=` variables:
//
// * `CRATON_GPU_ANNOTATIONS_JAR` - absolute path to the produced jar, or
//   empty string when the jar could not be produced.
// * `CRATON_GPU_ANNOTATIONS_DIR` - absolute path to a directory that either
//   contains the compiled `.class` files or is empty.
//
// The same two paths are also emitted as cargo build-script metadata via the
// `links = "craton-gpu-annotations"` declaration in `Cargo.toml`:
//
// * `cargo:annotations_dir=...`
// * `cargo:annotations_jar=...`
//
// Cargo exposes these to dependents' build scripts as env vars
// `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR` and
// `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR`. The `rustc-env` form alone
// is not enough; cargo intentionally does not propagate `rustc-env=` vars to
// dependents' build scripts.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Java sources moved to a standalone Maven project, the gpu4j repo
    // (formerly craton-gpu-java). They are not shipped inside this crate.
    // Locate them via, in priority order:
    //   1. $CRATON_GPU_JAVA_SRC env var (full absolute path to a directory
    //      containing `craton/gpu/*.java`); a set-but-invalid value is
    //      diagnosed via cargo:warning and then ignored,
    //   2. gpu4j/src/main/java (then craton-gpu-java/...) beside the
    //      CratonVM workspace checkout (portable sibling checkout; tried
    //      on every platform),
    //   3. C:/craton/gpu4j, then C:/craton/gpu-java, then
    //      C:/craton/craton-gpu-java (Windows-only defaults; never
    //      consulted on Linux/macOS).
    // Each checkout root is probed in every source layout the project has
    // had: the current <repo>/gpu4j-core/src/main/java, the 2026-08-28
    // aggregator <repo>/craton-gpu/src/main/java, and the pre-0.3.0 flat
    // <repo>/src/main/java. See `first_existing_layout`.
    // If none exists, the build script emits empty paths and a warning. The
    // build never fails.
    println!("cargo:rerun-if-env-changed=CRATON_GPU_JAVA_SRC");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let classes_dir = out_dir.join("classes");
    let jar_path = out_dir.join("craton-gpu-annotations.jar");

    // Helper: emit all four lines (two rustc-env, two cargo metadata) and
    // return. The rustc-env lines feed `env!()` in this crate's own
    // `src/lib.rs`; the `cargo:annotations_*` lines feed
    // `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_*` in dependents' build.rs
    // through the `links` key in Cargo.toml.
    let emit_env = |jar: &str, dir: &Path| {
        println!("cargo:rustc-env=CRATON_GPU_ANNOTATIONS_JAR={}", jar);
        println!(
            "cargo:rustc-env=CRATON_GPU_ANNOTATIONS_DIR={}",
            dir.display()
        );
        // Build-script metadata for dependents. Empty strings are fine;
        // dependents must tolerate them.
        println!("cargo:annotations_jar={}", jar);
        println!("cargo:annotations_dir={}", dir.display());
    };

    // Always reset the classes dir so stale .class files from an earlier
    // successful build cannot leak through failure or empty-source paths.
    if let Err(e) = prepare_clean_dir(&classes_dir) {
        println!(
            "cargo:warning=craton-gpu: failed to reset {}: {}; annotations will not be compiled",
            classes_dir.display(),
            e
        );
        let fallback_dir = prepare_clean_fallback_dir(&out_dir);
        emit_env("", &fallback_dir);
        return;
    }

    if let Err(e) = remove_file_if_exists(&jar_path) {
        println!(
            "cargo:warning=craton-gpu: failed to remove stale jar {}: {}",
            jar_path.display(),
            e
        );
    }

    let java_root = resolve_java_root();
    // Tell cargo to rerun when the chosen source tree changes. Only emit this
    // when `java_root` actually exists: `resolve_java_root` returns a default
    // candidate even when nothing is present, and cargo treats a missing
    // `rerun-if-changed` path as perpetually dirty.
    if java_root.is_dir() {
        println!("cargo:rerun-if-changed={}", java_root.display());
    }

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
    sources.sort();

    // Re-run when any individual `.java` source changes. The directory
    // `rerun-if-changed` above is not enough: on many platforms a directory's
    // mtime does not change when a file inside it is edited.
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
        Ok(status) if status.success() => {}
        Ok(status) => {
            println!(
                "cargo:warning=craton-gpu: javac exited with {}; annotations not packaged",
                status
            );
            let empty_dir = clean_failed_generation_dir(&classes_dir, &out_dir);
            emit_env("", &empty_dir);
            return;
        }
        Err(e) => {
            println!("cargo:warning=craton-gpu: failed to invoke javac: {}", e);
            let empty_dir = clean_failed_generation_dir(&classes_dir, &out_dir);
            emit_env("", &empty_dir);
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
/// 1. `$CRATON_GPU_JAVA_SRC`, treated as an absolute path to a directory
///    containing `craton/gpu/*.java`. A set-but-invalid value is diagnosed via
///    `cargo:warning=` and then ignored.
/// 2. A `gpu4j` (or legacy `craton-gpu-java`) checkout beside the CratonVM
///    workspace. This is portable and tried on every platform.
/// 3. `C:/craton/gpu4j`, `C:/craton/gpu-java` or
///    `C:/craton/craton-gpu-java`, default install paths consulted only on
///    Windows.
///
/// Returns the first path that exists. If none exists, returns a
/// platform-appropriate fallback; the caller discovers the absence and emits a
/// `cargo:warning=` instead of failing the build.
fn resolve_java_root() -> PathBuf {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let resolution = resolve_java_root_from(
        std::env::var_os("CRATON_GPU_JAVA_SRC"),
        &manifest_dir,
        cfg!(windows),
    );
    if let Some(p) = resolution.invalid_override {
        println!(
            "cargo:warning=craton-gpu: $CRATON_GPU_JAVA_SRC is set to {} but that is not a directory; ignoring the override and falling back to the default source-resolution candidates",
            p.display()
        );
    }
    // AUDIT 2026-09-02: say when the build depended on a path that is
    // true of one machine.
    //
    // Candidate 3 is an absolute install path — `C:/craton/gpu4j`,
    // named in `platform_fallback_java_root` after the box it was
    // written on. When it is what resolved, the annotation classes in
    // this build came from a directory nothing in this repository
    // records, at a revision nothing in this repository pins. That is a
    // reproducibility hazard worth one line of build output: the same
    // `cargo build` on another machine produces a DIFFERENT artifact,
    // or an empty one, and today says nothing either way.
    //
    // Not an error. A developer on that box is doing nothing wrong, and
    // failing the build would be worse than the problem. But the two
    // reproducible resolutions — the env var and the sibling checkout —
    // stay silent, so the warning only appears when it is telling you
    // something you could not otherwise know.
    if resolution.used_absolute_fallback && resolution.path.is_dir() {
        println!(
            "cargo:warning=craton-gpu: annotation sources resolved from the machine-specific \
             install path {}. This build is not reproducible elsewhere — the gpu4j \
             revision is not pinned by this repository. Set $CRATON_GPU_JAVA_SRC, or check the \
             project out beside the workspace, to make the source explicit.",
            resolution.path.display()
        );
    }
    resolution.path
}

#[derive(Debug, Eq, PartialEq)]
struct JavaRootResolution {
    path: PathBuf,
    invalid_override: Option<PathBuf>,
    /// The path came from [`platform_fallback_java_root`]'s absolute
    /// install locations rather than from the env override or the
    /// sibling checkout — i.e. from a convention true of one machine.
    /// See `resolve_java_root` for what is reported and why it is not an
    /// error.
    used_absolute_fallback: bool,
}

fn resolve_java_root_from(
    env_override: Option<OsString>,
    manifest_dir: &Path,
    is_windows: bool,
) -> JavaRootResolution {
    if let Some(v) = env_override {
        let override_path = PathBuf::from(v);
        if override_path.is_dir() {
            return JavaRootResolution {
                path: override_path,
                invalid_override: None,
                used_absolute_fallback: false,
            };
        }

        let sibling = documented_sibling_java_root(manifest_dir);
        if sibling.is_dir() {
            return JavaRootResolution {
                path: sibling,
                invalid_override: Some(override_path),
                used_absolute_fallback: false,
            };
        }

        return JavaRootResolution {
            path: platform_fallback_java_root(sibling, is_windows),
            invalid_override: Some(override_path),
            used_absolute_fallback: is_windows,
        };
    }

    let sibling = documented_sibling_java_root(manifest_dir);
    if sibling.is_dir() {
        return JavaRootResolution {
            path: sibling,
            invalid_override: None,
            used_absolute_fallback: false,
        };
    }

    JavaRootResolution {
        path: platform_fallback_java_root(sibling, is_windows),
        invalid_override: None,
        used_absolute_fallback: is_windows,
    }
}

/// Every name the Java project's checkout directory has had, newest
/// first. Tried as siblings of the workspace and, on Windows, under the
/// absolute install root — see [`platform_fallback_java_root`].
///
/// 2026-09-06: the repository was renamed from `craton-gpu-java` to
/// `gpu4j`. The old name stays here for the same reason the old source
/// layouts stay in [`first_existing_layout`]: a checkout of either
/// vintage has to keep resolving, because failing to resolve is silent.
const CHECKOUT_NAMES: [&str; 2] = ["gpu4j", "craton-gpu-java"];

fn documented_sibling_java_root(manifest_dir: &Path) -> PathBuf {
    let workspace_root = manifest_dir.parent().unwrap_or(manifest_dir);
    let workspace_parent = workspace_root.parent().unwrap_or(workspace_root);
    for name in CHECKOUT_NAMES {
        let candidate = first_existing_layout(&workspace_parent.join(name));
        if candidate.is_dir() {
            return candidate;
        }
    }
    // Nothing resolved: name the path a current checkout would use, so the
    // `cargo:warning` points somewhere actionable.
    first_existing_layout(&workspace_parent.join(CHECKOUT_NAMES[0]))
}

/// Every source layout a gpu4j (formerly craton-gpu-java) checkout can
/// have, newest first.
///
/// Three vintages, and this build script has to keep working against a
/// checkout of any of them, because the failure it would otherwise
/// produce is invisible: when no candidate resolves the build does not
/// fail, it emits an empty annotations directory and a `cargo:warning`,
/// which is easy to miss and leaves a VM that silently recognises no
/// `@GpuKernel` at all.
///
/// * `<repo>/gpu4j-core/src/main/java` — since 2026-09-06, when the
///   repository and its modules were renamed to gpu4j.
/// * `<repo>/craton-gpu/src/main/java` — from 2026-08-28, when that repo
///   became a Maven aggregator.
/// * `<repo>/src/main/java` — before that, when the repository root was
///   itself the module.
///
/// Returns the newest layout when none exists, so the `cargo:warning`
/// names the path a current checkout would use.
fn first_existing_layout(checkout: &Path) -> PathBuf {
    let current = checkout
        .join("gpu4j-core")
        .join("src")
        .join("main")
        .join("java");
    if current.is_dir() {
        return current;
    }
    let aggregator = checkout
        .join("craton-gpu")
        .join("src")
        .join("main")
        .join("java");
    if aggregator.is_dir() {
        return aggregator;
    }
    let flat = checkout.join("src").join("main").join("java");
    if flat.is_dir() {
        return flat;
    }
    current
}

fn platform_fallback_java_root(sibling: PathBuf, is_windows: bool) -> PathBuf {
    if is_windows {
        // The real checkout lives at C:/craton/gpu-java on this box — a
        // directory name that matches neither the old repository name nor
        // the new one — so it is kept as a candidate alongside both. Each
        // root is tried in every source layout; see `first_existing_layout`.
        for root in [
            "C:/craton/gpu4j",
            "C:/craton/gpu-java",
            "C:/craton/craton-gpu-java",
        ] {
            let candidate = first_existing_layout(&PathBuf::from(root));
            if candidate.is_dir() {
                return candidate;
            }
        }
        // Nothing resolved: name the path a current checkout would use, so
        // the `cargo:warning` points somewhere actionable.
        return first_existing_layout(&PathBuf::from("C:/craton/gpu4j"));
    }
    sibling
}

fn prepare_clean_dir(dir: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir_all(dir)
}

fn prepare_clean_fallback_dir(out_dir: &Path) -> PathBuf {
    let fallback_dir = out_dir.join("empty-classes");
    if let Err(e) = prepare_clean_dir(&fallback_dir) {
        println!(
            "cargo:warning=craton-gpu: failed to create clean fallback annotations dir {}: {}",
            fallback_dir.display(),
            e
        );
    }
    fallback_dir
}

fn clean_failed_generation_dir(classes_dir: &Path, out_dir: &Path) -> PathBuf {
    match prepare_clean_dir(classes_dir) {
        Ok(()) => classes_dir.to_path_buf(),
        Err(e) => {
            println!(
                "cargo:warning=craton-gpu: failed to clear generated classes after javac failure at {}: {}",
                classes_dir.display(),
                e
            );
            prepare_clean_fallback_dir(out_dir)
        }
    }
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Probe for `javac` on PATH by running `javac -version`. Both stdout and
/// stderr are discarded; only the exit status matters.
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
        } else if ftype.is_file() && path.extension().map(|e| e == "java").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

/// Invoke `jar --create --file <jar> -C <classes_dir> .`.
///
/// NOTE (reproducibility): the JDK `jar` tool embeds each entry's file
/// modification timestamp into the archive, so the produced jar is not
/// byte-reproducible across builds even when the `.class` inputs are
/// identical. There is no portable `jar` flag to normalize timestamps across
/// the supported JDK range, so downstream consumers that cache on content
/// should key on the compiled class directory rather than on the hash of this
/// jar.
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
