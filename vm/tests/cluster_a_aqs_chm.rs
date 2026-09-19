// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cluster A — AQS / ConcurrentHashMap park-livelock regression.
//!
//! Pins the fix for `LockSupport.parkNanos(long)` (and the `(Object,long)`,
//! `parkUntil`, `Unsafe.park(Z,J)` siblings) returning instantly when the
//! `long` argument arrives on the operand stack as a tag-erased
//! `Value::Double`. Pre-fix, the native registration matched only
//! `Value::Long(_)`, so any parkNanos call decoded `nanos == 0`, the guard
//! `if nanos > 0` skipped the actual `ctx.park`, and the JDK AQS
//! acquire loop went into a busy-spin (each unsuccessful tryAcquire
//! "parked" for 0 ns and immediately retried). Manifested as a 45 s
//! watchdog hang in `apps/aqs_probe/AqsProbe` and `apps/aqs_stress/AqsStress`.
//!
//! Acceptance: `AqsProbe` (16 threads × 10 000 ReentrantLock acquire/release
//! around a shared `AtomicInteger`) completes within 30 s with the final
//! marker `counter=160000` followed by `OK`. A regression in any of the
//! park natives surfaces as the subprocess timing out (livelock) or
//! `counter < 160000` (lost increments because the lock was released
//! mid-update).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("aqs_probe")
        .join("classes")
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

/// Compile `AqsProbe.java` via `javac` if `AqsProbe.class` is missing.
/// Best-effort: returns false if javac is unavailable or compilation fails.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("AqsProbe.class");
    if class_file.exists() {
        return true;
    }
    // Source lives one level up (apps/aqs_probe/AqsProbe.java); class file goes
    // into apps/aqs_probe/classes/.
    let source = dir.parent().unwrap().join("AqsProbe.java");
    if !source.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "cluster_a_aqs_chm",
            "the Cluster A fixture `AqsProbe.java`",
            &[source.clone()],
        );
        return false;
    }
    let _ = std::fs::create_dir_all(&dir);
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
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
                        "[cluster_a_aqs_chm] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[cluster_a_aqs_chm] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

fn run_aqs_probe(timeout: Duration) -> Option<(String, String)> {
    if !ensure_probe_compiled() {
        eprintln!(
            "[cluster_a_aqs_chm] AqsProbe.class unavailable (javac on PATH?); \
             skipping"
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[cluster_a_aqs_chm] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let dir = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&dir)
        .arg("AqsProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[cluster_a_aqs_chm] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[cluster_a_aqs_chm] AqsProbe timed out after {timeout:?} \
                         — likely LockSupport.park livelock regression"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[cluster_a_aqs_chm] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[cluster_a_aqs_chm] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// 16 threads × 10 000 ReentrantLock acquire/release with a shared
/// AtomicInteger increment under the lock must commit every increment
/// (final counter == 160 000) and complete within 30 s. A LockSupport.park
/// regression surfaces as the subprocess timing out (busy-spin in AQS
/// queue) or the counter going below 160 000 (lock released mid-update).
#[test]
fn aqs_probe_no_park_livelock() {
    let (stdout, stderr) = match run_aqs_probe(Duration::from_secs(30)) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("counter=160000"),
        "AqsProbe never produced 'counter=160000' — LockSupport.park \
         likely returns instantly, AQS busy-spins. Output:\n{combined}"
    );
    assert!(
        combined.contains("\nOK\n")
            || combined.trim_end().ends_with("OK")
            || combined.contains("\nOK\r\n"),
        "AqsProbe never reached final 'OK' marker. Output:\n{combined}"
    );
}
