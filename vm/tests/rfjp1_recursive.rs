// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RFJP.1 — `apps/fjp_probe/FjpProbe` (`pool.invoke(RecursiveTask)` of a
//! divide-and-conquer Long sum) must print `sum = 499999500000`, `depth = 10`
//! and `OK`.
//!
//! Pre-fix the JIT compiled the recursive `compute()` returning Long and
//! a regalloc clobber at depth >= 10 caused the probe to print `sum = 0`.
//! The workaround landed in `vm/src/runtime/interpreter.rs::class_extends_forkjointask`,
//! which forces interpreter-only execution for any method whose declaring
//! class is in the ForkJoinTask family
//! (ForkJoinTask / RecursiveTask / RecursiveAction / CountedCompleter).
//! It was subsequently root-caused to a `Long.valueOf` boxing miscompile
//! (Session 108): once boxing stopped folding to `value=0`, the
//! divide-and-conquer sum returned correctly, and both earlier failure modes
//! (regalloc clobber + interpreter stack overflow) turned out to be downstream
//! symptoms of it.
//!
//! Why this is a real-JDK-mode pin (not a `vm.invoke` Rust unit test):
//!   * The bug only reproduces when JIT-compiled bytecode of
//!     `RecursiveTask.compute()` runs against the real Adoptium 25
//!     ForkJoinTask hierarchy. Synthetic-JDK mode has its own stub
//!     implementations and does not exercise the JIT regalloc path.
//!   * Therefore the test spawns the RELEASE `cratonvm` binary as a subprocess.
//!     That is the one thing this file pins that its sibling
//!     `fjp_recursive.rs` does not: `fjp_recursive` accepts a debug build,
//!     this one does not, because the miscompile was profile-sensitive.
//!
//! # This test was VACUOUS from some point until 2026-08-07
//!
//! `fjp_probe_classes()` returned `None` whenever `apps/fjp_probe/classes/`
//! held no `.class` files — and it never compiled them, and the source
//! `apps/fjp_probe/FjpProbe.java` was **absent from the tree** entirely. So on
//! any checkout the body printed "Skipping: ... FjpProbe.class missing" and
//! cargo reported `ok` in 0.00 s. It asserted nothing, and it was found only by
//! accident.
//!
//! Two things changed:
//!
//! * this test now COMPILES the fixture itself instead of hoping someone else
//!   left class files behind — the previous shape could only ever pass by luck;
//! * **a missing fixture SOURCE is a PANIC, not a skip.** A checked-in fixture
//!   that has vanished is a broken repository, not an absent toolchain. Only a
//!   `javac` that cannot be LAUNCHED, a missing release binary, or a missing JDK
//!   still skip — and `CRATONVM_REQUIRE_E2E=1` promotes all of those to
//!   failures.
//!
//! `apps/` is listed in `.gitignore`, which is the mechanism by which the
//! fixture disappeared. The lookup below therefore also accepts the tracked
//! `probes/FjpProbe.java`, so relocating the fixture needs no change here.
//! Class files are written under `target/`, in a directory keyed to THIS test
//! binary, so the two RFJP.1 harnesses cannot race each other's `javac`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod common;

/// Tag used in every diagnostic this file emits.
const TAG: &str = "rfjp1_recursive";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

/// Locate the checked-in probe source. **Panics** when it is absent: this is a
/// fixture the repository is supposed to carry, and returning `None` here is
/// exactly what made this test a permanent green.
fn probe_source() -> PathBuf {
    let root = workspace_root();
    // The TRACKED copy first. `3b2901531` moved the probe corpus to
    // `apps/probes/`, force-added past `.gitignore` line 12; a curated 32 went
    // with it and the rest were dropped. `FjpProbe.java` was not among the 32
    // and — this is the part that made the harness lie — was never tracked at
    // `probes/` either, despite the message below having named that the durable
    // home since 2026-08-07. So both candidates were untracked, this test
    // resolved only against the gitignored `apps/fjp_probe/` copy, and it
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
         Restore it (it must print `sum = 499999500000`, `depth = 10` and `OK`, and exit non-zero \
         otherwise). If you moved it, add the new path to `probe_source`. Note `apps/` is \
         gitignored, so the tracked copy at `apps/probes/FjpProbe.java` was force-added and \
         must stay that way — `git add -f` it if you ever replace it.",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Per-test-binary output directory under `target/`.
fn probe_classes_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join("fjp-probe-classes")
        .join(TAG)
}

/// Prefer a JDK-relative `javac` over whatever is on `PATH`.
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

/// Compile the fixture into [`probe_classes_dir`]. Returns false ONLY when a
/// Java compiler could not be launched or cannot target the required release —
/// every other outcome is either success or a panic.
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
    let out = match Command::new(&javac)
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
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
            return false;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac never opened the file. That is a missing-toolchain
    // condition, not a broken probe — a genuine source error still reaches the
    // assertion below and still fails loudly (see `probe_compile_guard.rs`).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            if common::require_e2e() {
                panic!(
                    "[{TAG}] {} is set, but javac cannot target --release 21 ({}), so this test \
                     would have skipped and still reported `ok`.",
                    common::REQUIRE_VAR,
                    stderr_probe.lines().next().unwrap_or("").trim()
                );
            }
            eprintln!(
                "[{TAG}] javac cannot target --release 21 ({}); skipping. Point JAVA_HOME or \
                 CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return false;
        }
    }
    assert!(
        out.status.success(),
        "[{TAG}] the checked-in probe fixture {} failed to compile — fix the .java source. javac \
         stderr:\n{}",
        src.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        main_class.exists() && task_class.exists(),
        "[{TAG}] javac reported success but {} / {} are absent. The probe's class or nested-task \
         name changed; both RFJP.1 harnesses gate on `FjpProbe` and `FjpProbe$SumTask`.",
        main_class.display(),
        task_class.display()
    );
    true
}

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

/// Path to the freshly-built RELEASE CLI binary. Honors `CRATONVM_BIN` for
/// callers pointing at a custom build; otherwise resolves to the workspace's
/// `target/release/cratonvm[.exe]`. Deliberately does NOT fall back to `debug`
/// — see the module header.
fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CRATONVM_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    let candidate = workspace_root().join("target").join("release").join(exe);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Prerequisite gate: see `common::require_jdk`.
fn real_java_home() -> Option<PathBuf> {
    common::require_jdk(real_java_home_lookup())
}

/// Resolve the real-JDK 25 java-home — env vars first, then the standard
/// Adoptium install path. Returns None when none is reachable so the test can
/// skip cleanly on machines without the JDK 25 dependency.
fn real_java_home_lookup() -> Option<PathBuf> {
    for var in ["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(jh) = std::env::var(var) {
            let p = PathBuf::from(&jh);
            if p.join("bin/java.exe").exists() || p.join("bin/java").exists() {
                return Some(p);
            }
        }
    }
    let adoptium = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if adoptium.join("bin/java.exe").exists() {
        return Some(adoptium);
    }
    None
}

/// RFJP.1 acceptance: spawn the release cratonvm in real-JDK mode against
/// `FjpProbe`, assert stdout contains `sum = 499999500000`, `depth = 10` and
/// `OK`.
#[test]
fn fjp_probe_recursive_returns_correct_sum() {
    if !ensure_probe_compiled() {
        eprintln!("[{TAG}] probe could not be compiled (no usable javac); skipping");
        return;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[{TAG}] Skipping: cratonvm RELEASE binary not available at \
                 target/release/cratonvm[.exe] (build with `cargo build --release -p \
                 cratonvm-cli`)"
            );
            return;
        }
    };
    let java_home = match real_java_home() {
        Some(h) => h,
        None => {
            eprintln!(
                "[{TAG}] Skipping: real JDK 25 not available (set CRATONVM_TEST_JDK / JAVA_HOME, \
                 or install Adoptium 25 at the documented path)"
            );
            return;
        }
    };
    let classes = probe_classes_dir();

    let mut child = match Command::new(&bin)
        .arg("--java-home")
        .arg(&java_home)
        .arg("-c")
        .arg(&classes)
        .arg("FjpProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => panic!("[{TAG}] failed to spawn {}: {e}", bin.display()),
    };
    // Bounded. The pre-fix failure mode included an interpreter stack overflow
    // and a starving pool, either of which can hang; an unbounded `.output()`
    // would turn that into a wedged CI job instead of a named failure.
    let timeout = Duration::from_secs(120);
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[{TAG}] FjpProbe did not finish within {timeout:?}. A deeply-recursive \
                         RecursiveTask that never completes is the ForkJoin starvation / host \
                         stack-overflow shape, not slowness."
                    );
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
        "FjpProbe stdout missing `sum = 499999500000`. The RFJP.1 workaround in \
         `try_jit_compile_callee` / `try_jit_upgrade_with_gate` may not be excluding \
         ForkJoinTask subclasses from JIT — the recursive Long compute() is hitting the \
         boxing/regalloc miscompile and returning 0.\nstdout: {stdout}\nstderr: {stderr}"
    );
    // The probe self-checks and prints its recursion depth. Pinning the exact
    // value keeps the SHAPE under test: a probe that stopped recursing would
    // still print the right sum, which is how this test can go quiet again.
    assert!(
        stdout.contains("depth = 10"),
        "FjpProbe did not recurse to depth 10 — the deep-RecursiveTask shape RFJP.1 is about is \
         gone.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "FjpProbe stdout missing `OK` marker.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "cratonvm exited non-zero: {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
}
