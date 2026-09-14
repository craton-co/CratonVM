// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for real-JDK StackWalker lazy frame-buffer fill under
//! moving-GC pressure. The fixture forces `StackStreamFactory$StackFrameBuffer`
//! to instantiate `StackFrameInfo(StackWalker)` reflectively while young GC is
//! stress-triggered, matching the Hibernate ByteArrayMappingTests crash shape.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "StackWalkerReflectGc";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
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
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = workspace_root().join("target").join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn classpath_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("resources"),
    );
    candidates.into_iter().find(|dir| {
        dir.join("cratonvm")
            .join(format!("{FIXTURE}.class"))
            .exists()
    })
}

#[test]
fn stackwalker_reflective_fill_survives_gc_stress() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[stackwalker_reflect_gc] cratonvm binary not found; skipping");
        return;
    };
    let Some(classpath) = classpath_dir() else {
        eprintln!("[stackwalker_reflect_gc] {FIXTURE}.class not compiled; skipping");
        return;
    };

    let mut child = Command::new(&bin)
        .arg("--nojit")
        .arg("--Xmx")
        .arg("96m")
        .arg("-c")
        .arg(&classpath)
        .arg(format!("cratonvm.{FIXTURE}"))
        .env("CRATONVM_DISABLE_DEFAULT_WATCHDOG", "1")
        .env("CRATONVM_GC_STRESS", "1048576")
        .env("CRATONVM_MOVING_YOUNG", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("[stackwalker_reflect_gc] failed to spawn {bin:?}: {e}"));

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > RUN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[stackwalker_reflect_gc] {FIXTURE} timed out after {RUN_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[stackwalker_reflect_gc] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("[stackwalker_reflect_gc] wait_with_output failed");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert!(
        output.status.success() && stdout.contains("STACKWALKER_REFLECT_GC_OK"),
        "{FIXTURE} failed under StackWalker GC stress.\nstatus={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout,
        stderr
    );
}
