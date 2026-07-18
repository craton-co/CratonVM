// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for SO_TIMEOUT on all SocketInputStream read overloads.

use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = "SocketInputStreamTimeout";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

fn cratonvm_binary() -> Option<PathBuf> {
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

fn run_probe(binary: &Path, nojit: bool, real_net_sockets: bool) {
    let mut command = Command::new(binary);
    if nojit {
        command.arg("--nojit");
    }
    if real_net_sockets {
        command.env("CRATONVM_REAL_NET_SOCKETS", "1");
    } else {
        command.env_remove("CRATONVM_REAL_NET_SOCKETS");
    }
    let output = command
        .arg("-c")
        .arg(workspace_root().join("vm/tests/resources"))
        .arg(format!("cratonvm.{FIXTURE}"))
        .output()
        .expect("failed to launch SocketInputStream timeout fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("SOCKET_INPUT_STREAM_TIMEOUT_OK"),
        "{FIXTURE} failed (nojit={nojit}, real_net_sockets={real_net_sockets}). stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn socket_input_stream_timeout_is_typed_and_never_eof() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[socket_input_stream_timeout] cratonvm binary not found; skipping");
        return;
    };
    for real_net_sockets in [false, true] {
        run_probe(&binary, true, real_net_sockets);
        run_probe(&binary, false, real_net_sockets);
    }
}
