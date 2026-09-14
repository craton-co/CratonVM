// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 1 Task C — ExecutorService / ThreadPoolExecutor / Executors / ThreadFactory /
//! UncaughtExceptionHandler regression test.
//!
//! Pins the runtime behavior of the four sub-tests in `apps/executor_probe/ExecProbe.java`
//! through the `cratonvm.exe` CLI binary against the real JDK 25:
//!
//!   * test1: `Executors.newFixedThreadPool(4).submit(() -> 42).get()` returns `42`.
//!   * test2: 4-thread x 4000-task throughput against an `AtomicInteger` + `CountDownLatch`
//!            (`latch.await(10, SECONDS)` returns `true`, counter == 4000).
//!   * test3: `Executors.newSingleThreadExecutor(threadFactory)` honors a custom
//!            `ThreadFactory` that names the worker `ExecProbe-worker`.
//!   * test4: `Thread.setUncaughtExceptionHandler` is invoked when the Runnable throws
//!            (W1-C fix: `dispatchUncaughtException` is invoked from `thread_start`
//!            on an `ExceptionThrown` result).
//!
//! Pattern adapted from `vm/tests/vthread_probe_regression.rs`. The binary path is
//! resolved via `CRATONVM_BIN` env var, then the cargo `target/{release,debug}` fallback.
//! Class files are produced on-demand via `javac --release 21` if absent. If neither
//! the binary nor `javac` is available the test reports `skipped` rather than failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("executor_probe")
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

fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("ExecProbe.class");
    let java_file = dir.join("ExecProbe.java");
    if class_file.exists() {
        return true;
    }
    if !java_file.exists() {
        // `apps/executor_probe/ExecProbe.java` IS tracked (force-added past the
        // `.gitignore` `apps/` rule), so its absence means a broken checkout, not
        // an absent toolchain. Loud, and a failure under CRATONVM_REQUIRE_E2E —
        // see `common::require_fixture`.
        let _ = common::require_fixture(
            "wave1_c_executor",
            "the Wave 1 Task C fixture `ExecProbe.java` (tracked at \
             apps/executor_probe/ExecProbe.java despite the `apps/` gitignore rule)",
            &[java_file.clone()],
        );
        return false;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&java_file)
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
                        "[wave1_c_executor] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wave1_c_executor] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

fn run_probe() -> Option<(String, String)> {
    if !ensure_probe_compiled() {
        eprintln!("[wave1_c_executor] ExecProbe class file unavailable; skipping");
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[wave1_c_executor] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let dir = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&dir)
        .arg("ExecProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave1_c_executor] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let timeout = Duration::from_secs(30);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[wave1_c_executor] ExecProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave1_c_executor] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave1_c_executor] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn cached_run() -> Option<(String, String)> {
    static OUT: OnceLock<Option<(String, String)>> = OnceLock::new();
    OUT.get_or_init(run_probe).clone()
}

#[test]
fn exec_probe_test1_basic_submit_get() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("test1=42"),
        "ExecProbe test1 expected 'test1=42'. Output:\n{combined}"
    );
}

#[test]
fn exec_probe_test2_throughput_4000_tasks() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("test2.latch=true counter=4000"),
        "ExecProbe test2 expected 'test2.latch=true counter=4000'. Output:\n{combined}"
    );
}

#[test]
fn exec_probe_test3_named_thread_factory() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("test3.threadName=ExecProbe-worker"),
        "ExecProbe test3 expected 'test3.threadName=ExecProbe-worker'. Output:\n{combined}"
    );
}

#[test]
fn exec_probe_test4_uncaught_exception_handler() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    // W1-C contract: setUncaughtExceptionHandler must fire when the Runnable
    // throws. The handler captures the Throwable into an AtomicReference, so
    // `caught.get() != null` <=> handler fired. The `msg=` portion is *not*
    // pinned here: `Throwable.getMessage()` on a synthetic-allocated
    // RuntimeException currently returns null due to slot-layout drift
    // (pre-existing, tracked separately under WP8.10.7). The W1-C fix is
    // strictly about the handler dispatch path in `thread_start`.
    assert!(
        combined.contains("test4.caught=true"),
        "ExecProbe test4 expected 'test4.caught=true' \
         (UncaughtExceptionHandler must fire when Runnable throws). Output:\n{combined}"
    );
}

#[test]
fn exec_probe_completes_with_ok_marker() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("\nOK\n")
            || combined.trim_end().ends_with("OK")
            || combined.contains("\nOK\r\n"),
        "ExecProbe never reached final 'OK' marker. Output:\n{combined}"
    );
}
