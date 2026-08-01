// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RFJP.1 — Recursive ForkJoin probe regression test.
//!
//! Pins the runtime behavior of deeply-recursive `RecursiveTask<Long>.compute()`
//! against the probe in `apps/fjp_probe/FjpProbe.java`. The probe spawns a
//! divide-and-conquer sum over a 1M-long array with a 1000-element threshold,
//! producing recursion depth ~10. Expected output:
//!
//!   sum = 499999500000
//!   OK
//!
//! When the JIT miscompiled `compute()` (RFJP.1 baseline), the probe printed
//! `sum = 0` and exited with `FAIL 499999500000`. The fix bails out of JIT
//! compilation for any class transitively extending
//! `java/util/concurrent/ForkJoinTask` so this probe runs in the interpreter
//! end-to-end.
//!
//! The binary path is resolved via `CRATONVM_BIN` env var, then the cargo
//! `target/{release,debug}` fallback. Class files are produced on-demand via
//! `javac --release 21` if absent. If neither the binary nor `javac` is
//! available the test reports `skipped` rather than failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().join("apps").join("fjp_probe")
}

fn probe_classes_dir() -> PathBuf {
    probe_dir().join("classes")
}

fn cratonvm_binary() -> Option<PathBuf> {
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
    let classes = probe_classes_dir();
    if classes.join("FjpProbe.class").exists() && classes.join("FjpProbe$SumTask.class").exists() {
        return true;
    }
    let _ = std::fs::create_dir_all(&classes);
    let src = probe_dir().join("FjpProbe.java");
    if !src.exists() {
        return false;
    }
    let status = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output();
    match status {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            assert!(
                o.status.success(),
                "[fjp_recursive] the checked-in probe fixture failed to compile — fix \
                 the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            classes.join("FjpProbe.class").exists()
                && classes.join("FjpProbe$SumTask.class").exists()
        }
    }
}

fn jdk_home() -> Option<PathBuf> {
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
        eprintln!("[fjp_recursive] FjpProbe.class unavailable; skipping");
        return;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[fjp_recursive] cratonvm binary not found; build with \
                 `cargo build --release -p cratonvm-cli`"
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!("[fjp_recursive] no JDK home (set CRATONVM_TEST_JDK or JAVA_HOME); skipping");
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
            eprintln!("[fjp_recursive] failed to spawn cratonvm: {e}");
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
                    panic!("[fjp_recursive] FjpProbe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[fjp_recursive] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains("sum = 499999500000"),
        "FjpProbe stdout missing expected sum line.\nstdout:\n{stdout}\nstderr:\n{stderr}"
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
