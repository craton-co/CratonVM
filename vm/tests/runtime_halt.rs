// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `Runtime.getRuntime().halt(status)` must terminate the process with that
//! status, immediately, without running shutdown hooks.
//!
//! It reaches two `java.lang.Shutdown` natives — `beforeHalt()` and, through
//! `Shutdown.halt`, `halt0(int)`. Neither was registered, so the first threw
//! `UnsatisfiedLinkError: java/lang/Shutdown.beforeHalt()V` out of a method
//! that cannot legally return: the caller got a linkage error and the process
//! kept running.

use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = "RuntimeHalt";

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
        let path = PathBuf::from(bin);
        if path.exists() {
            return Some(path);
        }
    }
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    ["release", "debug"]
        .into_iter()
        .map(|profile| workspace_root().join("target").join(profile).join(exe))
        .find(|path| path.exists())
}

fn run_halt(binary: &Path, status: i32) {
    let output = Command::new(binary)
        .arg("-c")
        .arg(workspace_root().join("vm/tests/resources"))
        .arg(format!("cratonvm.{FIXTURE}"))
        .arg(status.to_string())
        .output()
        .expect("failed to launch the Runtime.halt fixture");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(
        stdout.contains("BEFORE-HALT"),
        "fixture did not reach halt(). {context}"
    );
    assert!(
        !stdout.contains("HALT-RETURNED"),
        "halt({status}) RETURNED instead of terminating. {context}"
    );
    assert!(
        !stdout.contains("SHUTDOWN-HOOK-RAN"),
        "halt({status}) ran a shutdown hook; halt is forcible termination. {context}"
    );
    assert!(
        !stderr.contains("UnsatisfiedLinkError"),
        "halt({status}) hit an unregistered native. {context}"
    );
    assert_eq!(
        output.status.code(),
        Some(status),
        "halt({status}) exited with the wrong status. {context}"
    );
}

#[test]
fn runtime_halt_terminates_with_the_requested_status() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[runtime_halt] cratonvm binary not found; skipping");
        return;
    };
    // A non-zero status, so "exited 0 because nothing happened" cannot pass,
    // and 0, which is the value a `halt(0)` caller actually uses.
    run_halt(&binary, 3);
    run_halt(&binary, 0);
}
