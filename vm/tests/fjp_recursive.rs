// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RFJP.1 — Recursive ForkJoin probe regression test.
//!
//! Pins the runtime behavior of deeply-recursive `RecursiveTask<Long>.compute()`
//! against the probe in `apps/fjp_probe/FjpProbe.java`. The probe spawns a
//! divide-and-conquer sum over a 1M-long array with a 1000-element threshold,
//! producing recursion depth 10. Expected output:
//!
//!   sum = 499999500000
//!   depth = 10
//!   OK
//!
//! When the JIT miscompiled `compute()` (RFJP.1 baseline), the probe printed
//! `sum = 0` and exited with `FAIL 499999500000`. The fix bails out of JIT
//! compilation for any class transitively extending
//! `java/util/concurrent/ForkJoinTask` so this probe runs in the interpreter
//! end-to-end.
//!
//! # This test was VACUOUS from some point until 2026-08-07
//!
//! `apps/fjp_probe/FjpProbe.java` was **not in the tree**. `ensure_probe_compiled`
//! hit `if !src.exists() { return false; }`, the body printed
//! "FjpProbe.class unavailable; skipping", and cargo reported `ok` in 0.00 s.
//! Nothing here could ever fail. That is how the fixture stayed missing for so
//! long — and it is why the recorded justification for lazy ForkJoin fork
//! ("eager fork overflowed the host stack on deeply-recursive `RecursiveTask`")
//! had no live guard anywhere in the repo when that default was flipped; the
//! flip needed a bespoke A/B instead.
//!
//! Two things changed:
//!
//! * the fixture is restored (see its own header for what each constant pins);
//! * **a missing fixture is now a PANIC, not a skip.** A checked-in fixture that
//!   has vanished is a broken repository, not an absent toolchain. Only a `javac`
//!   that cannot be LAUNCHED, a missing `cratonvm` binary, or a missing JDK still
//!   skip — and `CRATONVM_REQUIRE_E2E=1` promotes all three to failures
//!   (`common::require_binary` / `common::require_jdk` / `common::require_e2e`).
//!
//! Note that `apps/` is listed in `.gitignore`, which is the mechanism by which
//! the fixture disappeared in the first place. The source lookup below therefore
//! also accepts `probes/FjpProbe.java` — `probes/` is tracked — so moving the
//! fixture there needs no change to this file.
//!
//! Compiled classes go under `target/`, one directory per test binary, so the
//! two RFJP.1 harnesses cannot race each other's `javac` output.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

/// Tag used in every diagnostic this file emits.
const TAG: &str = "fjp_recursive";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

/// Locate the checked-in probe source. **Panics** when it is absent: this is a
/// fixture the repository is supposed to carry, and the historical `return
/// false` here is precisely what made this test a permanent green.
fn probe_source() -> PathBuf {
    let root = workspace_root();
    // The TRACKED copy first. `3b2901531` moved the probe corpus to
    // `apps/probes/`, force-added past `.gitignore` line 12; a curated 32 went
    // with it and the rest were dropped. `FjpProbe.java` was not among the 32
    // and — this is the part that made the harness lie — was never tracked at
    // `probes/` either, despite the message below having named that the durable
    // home since 2026-08-07. So both candidates were untracked, these tests
    // resolved only against the gitignored `apps/fjp_probe/` copy, and they
    // panicked in any fresh worktree while passing on the machine that happened
    // to hold it.
    //
    // The two historical paths stay behind the new one so a checkout predating
    // the move still resolves; the tracked copy wins when both exist.
    let candidates = [
        root.join("apps").join("probes").join("FjpProbe.java"),
        root.join("apps").join("fjp_probe").join("FjpProbe.java"),
        root.join("probes").join("FjpProbe.java"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    panic!(
        "[{TAG}] the RFJP.1 fixture `FjpProbe.java` is MISSING. Looked in:\n  {}\n\nThis is NOT a \
         skippable prerequisite — it is a file this repository is supposed to carry, and until \
         2026-08-07 its absence made this test report `ok` in 0.00 s while asserting nothing. \
         Restore it (see the header of either RFJP.1 test for what it must print). If you moved \
         it, add the new path to `probe_source`. Note `apps/` is gitignored, so the tracked copy at \
         `apps/probes/FjpProbe.java` was force-added and must stay that way — \
         `git add -f` it if you ever replace it.",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Per-test-binary output directory under `target/`, so the two RFJP.1 harnesses
/// never write the same class files concurrently.
fn probe_classes_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join("fjp-probe-classes")
        .join(TAG)
}

/// Prefer a JDK-relative `javac` over whatever is on `PATH`, so the probe is
/// compiled by the same toolchain the run below uses.
fn javac_path() -> PathBuf {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    for var in ["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(var) {
            let candidate = PathBuf::from(home).join("bin").join(exe);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("javac")
}

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
    let target = workspace_root().join("target");
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

/// True when `class_file` exists and is at least as new as `src`.
fn up_to_date(class_file: &Path, src: &Path) -> bool {
    let (Ok(c), Ok(s)) = (class_file.metadata(), src.metadata()) else {
        return false;
    };
    match (c.modified(), s.modified()) {
        (Ok(c), Ok(s)) => c >= s,
        // No mtime on this filesystem: recompile rather than trust a stale class.
        _ => false,
    }
}

fn ensure_probe_compiled() -> bool {
    // Panics if the fixture is gone. Deliberately evaluated FIRST, so a missing
    // fixture is reported even on a machine with stale classes lying around.
    let src = probe_source();
    let classes = probe_classes_dir();
    let main_class = classes.join("FjpProbe.class");
    let task_class = classes.join("FjpProbe$SumTask.class");
    if main_class.exists() && task_class.exists() && up_to_date(&main_class, &src) {
        return true;
    }
    let _ = std::fs::create_dir_all(&classes);
    let javac = javac_path();
    let status = Command::new(&javac)
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output();
    match status {
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            if common::require_e2e() {
                panic!(
                    "[{TAG}] {} is set, but `{}` could not be executed ({e}), so this test would \
                     have skipped and still reported `ok`. Point CRATONVM_TEST_JDK or JAVA_HOME \
                     at a JDK 21+ install.",
                    common::REQUIRE_VAR,
                    javac.display()
                );
            }
            eprintln!(
                "[{TAG}] `{}` could not be executed ({e}); skipping.",
                javac.display()
            );
            false
        }
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
                    if common::require_e2e() {
                        panic!(
                            "[{TAG}] {} is set, but javac cannot target --release 21 ({}), so \
                             this test would have skipped and still reported `ok`.",
                            common::REQUIRE_VAR,
                            stderr_probe.lines().next().unwrap_or("").trim()
                        );
                    }
                    eprintln!(
                        "[{TAG}] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[{TAG}] the checked-in probe fixture {} failed to compile — fix the .java \
                 source. javac stderr:\n{}",
                src.display(),
                String::from_utf8_lossy(&o.stderr)
            );
            // javac reported success, so the two class files MUST be there. If
            // they are not, the output directory is not what we think it is —
            // another silent-skip shape, so say it out loud.
            assert!(
                main_class.exists() && task_class.exists(),
                "[{TAG}] javac reported success but {} / {} are absent. The probe's class or \
                 nested-task name changed; both RFJP.1 harnesses gate on `FjpProbe` and \
                 `FjpProbe$SumTask`.",
                main_class.display(),
                task_class.display()
            );
            true
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

#[test]
fn fjp_probe_recursive_returns_correct_sum() {
    if !ensure_probe_compiled() {
        eprintln!("[{TAG}] probe could not be compiled (no javac); skipping");
        return;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[{TAG}] cratonvm binary not found; build with \
                 `cargo build --release -p cratonvm-cli`"
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!("[{TAG}] no JDK home (set CRATONVM_TEST_JDK or JAVA_HOME); skipping");
            return;
        }
    };
    let classes = probe_classes_dir();
    let mut child = match Command::new(&bin)
        .arg("--java-home")
        .arg(&jdk)
        .arg("-c")
        .arg(&classes)
        .arg("FjpProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{TAG}] failed to spawn cratonvm: {e}");
            return;
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
                    panic!("[{TAG}] FjpProbe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[{TAG}] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains("sum = 499999500000"),
        "FjpProbe stdout missing expected sum line.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The probe self-checks its recursion depth and prints it. Pinning the exact
    // value here keeps the *shape* under test: a probe that stopped recursing
    // would still print the right sum.
    assert!(
        stdout.contains("depth = 10"),
        "FjpProbe did not recurse to depth 10 — the deep-RecursiveTask shape this test exists \
         to exercise is gone.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "FjpProbe stdout missing OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "FjpProbe exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}
