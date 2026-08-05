// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ServerSocketChannel.socket().accept()` — the `java.net.ServerSocket` view
//! of a channel — must accept connections and honour `setSoTimeout`, in BOTH
//! socket modes.
//!
//! The two modes are different implementations. Under real sockets (the
//! default) the adapter is the JDK's own `ServerSocketAdaptor` over
//! `sun/nio/ch/Net`. Under the legacy synthetic surface
//! (`CRATONVM_REAL=-net-sockets`) it is a CratonVM `ServerSocket` carrying a
//! back-ref to a CratonVM channel — and its methods are split across two
//! crates that cannot call each other, which is what broke it: `accept` is not
//! one of the six methods native-io wraps, so it reached net_phase_e's RE.2
//! native, saw `listener_id = -1` (the listener lives in the channel registry)
//! and reported `IOException: ServerSocket not bound`.
//!
//! The guarded failure modes include a hang (an ignored SO_TIMEOUT), so the
//! child process is bounded here as well as inside the fixture.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "ServerSocketAdaptorAccept";
const MARKER: &str = "SERVER_SOCKET_ADAPTOR_ACCEPT_OK";
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

fn run_fixture(binary: &Path, synthetic_sockets: bool) {
    let mut command = Command::new(binary);
    if synthetic_sockets {
        command.env("CRATONVM_REAL", "-net-sockets");
    } else {
        command.env_remove("CRATONVM_REAL");
    }
    command.env_remove("CRATONVM_SYNTHETIC_NET_SOCKETS");
    command.env_remove("CRATONVM_REAL_NET_SOCKETS");

    let mut child = command
        .arg("-c")
        .arg(workspace_root().join("vm/tests/resources"))
        .arg(format!("cratonvm.{FIXTURE}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to launch the ServerSocket adaptor fixture");

    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        match child.try_wait().expect("failed to poll the fixture") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "{FIXTURE} (synthetic_sockets={synthetic_sockets}) did not finish within \
                     {RUN_TIMEOUT:?}; an accept is blocked"
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let output = child
        .wait_with_output()
        .expect("failed to collect the fixture output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains(MARKER),
        "{FIXTURE} failed (synthetic_sockets={synthetic_sockets}). stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn server_socket_adaptor_accepts_in_both_socket_modes() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[server_socket_adaptor_accept] cratonvm binary not found; skipping");
        return;
    };
    for synthetic_sockets in [false, true] {
        run_fixture(&binary, synthetic_sockets);
    }
}
