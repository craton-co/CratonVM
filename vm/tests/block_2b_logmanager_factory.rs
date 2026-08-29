// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Block 2B — `java.util.logging.LogManager.getLogManager()` honours the
//! `-Djava.util.logging.manager=<className>` system property.
//!
//! The `apps/lm_subclass/LmSubclass` probe exercises both:
//!   * the default case (no property set) —
//!     `getLogManager().getClass().getName()` is `java.util.logging.LogManager`,
//!   * the property-driven case
//!     (`-Djava.util.logging.manager=LmSubclass$MyLm`) —
//!     `MyLm-init` is printed by the subclass ctor and the same
//!     `getClass().getName()` returns `LmSubclass$MyLm`.
//!
//! The probe is run as a subprocess against the cratonvm CLI binary.
//! Skips cleanly with a diagnostic if the binary, the compiled class
//! files, or the real JDK are unavailable so CI doesn't hard-fail when
//! the worktree hasn't been built or when JDK 25 isn't installed at the
//! expected path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().join("apps").join("lm_subclass")
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Is either compiled class OLDER than the fixture source?
///
/// A `.class` that predates its `.java` is the documented stale-artefact trap:
/// the compiled probe keeps answering a question the current source no longer
/// asks, so an edit to the fixture appears in no log. An unreadable timestamp
/// (no source at all, no metadata) is not evidence of staleness — and cannot be
/// repaired by recompiling either — so it reads as `false` and any existing
/// artefact is kept.
fn any_class_older_than_source(src: &Path, classes: &[&Path]) -> bool {
    let Ok(src_mtime) = src.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    classes
        .iter()
        .any(|c| match c.metadata().and_then(|m| m.modified()) {
            Ok(cls_mtime) => cls_mtime < src_mtime,
            // Cannot tell — recompile rather than trust it.
            Err(_) => true,
        })
}

fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let cls = dir.join("LmSubclass.class");
    let inner = dir.join("LmSubclass$MyLm.class");
    let src = dir.join("LmSubclass.java");
    if cls.exists()
        && inner.exists()
        && !any_class_older_than_source(&src, &[cls.as_path(), inner.as_path()])
    {
        return true;
    }
    if !src.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`. `apps/` is gitignored (.gitignore line 12),
        // which is why this fixture was never tracked and is absent here.
        let _ = common::require_fixture(
            "block_2b_logmanager",
            "the Block 2B fixture `LmSubclass.java` (a LogManager subclass; the test pins its \
             LmSubclass / LmSubclass$MyLm class pair)",
            &[src.clone()],
        );
        return false;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&src)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[block_2b_logmanager_factory] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[block_2b_logmanager] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            cls.exists() && inner.exists()
        }
    }
}

/// Prerequisite gate: the lookup below is unchanged — only a MISSING JDK is
/// reported differently. See `common::require_jdk`.
fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    if let Ok(j) = std::env::var("CRATONVM_TEST_JDK") {
        let p = PathBuf::from(&j);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(j) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&j);
        if p.exists() {
            return Some(p);
        }
    }
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default.exists() {
        return Some(default);
    }
    None
}

/// Run the LmSubclass probe with the given extra args (e.g. a -D
/// property override) and capture stdout+stderr. Returns
/// `Some((stdout, stderr, success))` or `None` if a prerequisite is
/// missing (test should skip).
fn run_probe(extra_args: &[&str]) -> Option<(String, String, bool)> {
    if !ensure_probe_compiled() {
        eprintln!("[block_2b] LmSubclass.class unavailable; skipping");
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[block_2b] cratonvm binary not found; build with \
                 `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!("[block_2b] no JDK home (set CRATONVM_TEST_JDK or JAVA_HOME); skipping");
            return None;
        }
    };
    let classes = probe_dir();
    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home").arg(&jdk);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg("-c")
        .arg(&classes)
        .arg("LmSubclass")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[block_2b] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[block_2b] LmSubclass timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[block_2b] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    Some((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    ))
}

#[test]
fn block_2b_default_log_manager_when_property_unset() {
    let Some((stdout, stderr, success)) = run_probe(&[]) else {
        return;
    };
    assert!(
        stdout.contains("class=java.util.logging.LogManager"),
        "expected default class line.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "expected OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("MyLm-init"),
        "MyLm-init must NOT print without -Djava.util.logging.manager.\nstdout:\n{stdout}"
    );
    assert!(
        success,
        "default-case probe must exit zero.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn block_2b_subclass_log_manager_honours_system_property() {
    let Some((stdout, stderr, success)) =
        run_probe(&["-Djava.util.logging.manager=LmSubclass$MyLm"])
    else {
        return;
    };
    assert!(
        stdout.contains("MyLm-init"),
        "subclass ctor must run (MyLm-init line absent).\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("class=LmSubclass$MyLm"),
        "getClass().getName() must reflect the subclass.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "expected OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        success,
        "subclass-case probe must exit zero.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
