// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for JUL's parameterized logging bridge. A File in a
//! `Logger.log(Level, String, Object[])` parameter array must be rendered with
//! `toString`, never treated as a `Supplier` and sent a synthetic `get()` call.

use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = "ConscryptLoggerDispatch";

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

fn run_probe(binary: &Path, nojit: bool) {
    let mut command = Command::new(binary);
    if nojit {
        command.arg("--nojit");
    }
    let output = command
        .arg("-c")
        .arg(workspace_root().join("vm/tests/resources"))
        .arg(format!("cratonvm.{FIXTURE}"))
        .output()
        .expect("failed to launch Conscrypt logger dispatch fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("CONSCRYPT_LOGGER_DISPATCH_OK"),
        "{FIXTURE} failed (nojit={nojit}). stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("NoSuchMethodError"),
        "{FIXTURE} emitted a spurious virtual dispatch failure (nojit={nojit}):\n{stderr}"
    );
}

#[test]
fn conscrypt_logger_object_parameters_do_not_receive_supplier_get() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[conscrypt_logger_dispatch] cratonvm binary not found; skipping");
        return;
    };
    run_probe(&binary, true);
    run_probe(&binary, false);
}
