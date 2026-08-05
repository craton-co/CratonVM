// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for the plain (non-channel) `java.net.ServerSocket`
//! surface: `accept()` must honour SO_TIMEOUT set on either side of `bind()`,
//! and the lifecycle accessors must answer the way the real JDK's do — before
//! a bind, while bound, and after `close()`.
//!
//! The fixture asserts everything itself and passes on HotSpot; this harness
//! only runs it, once per socket mode.
//!
//! **Both modes matter and they are different code.** `real_net_sockets` is
//! DEFAULT-ON: the registry then drops every synthetic native on
//! `java/net/ServerSocket` and real JDK bytecode drives `sun/nio/ch/Net`
//! (native-io::net). Selecting the synthetic surface takes an explicit
//! `CRATONVM_REAL=-net-sockets`; *unsetting* `CRATONVM_REAL_NET_SOCKETS` does
//! NOT select it — a mistake that silently tests the default twice.
//!
//! The guarded failure mode is a HANG, so the run is bounded: without the fix
//! the fixture's own watchdog reports the blocked `accept()` and exits
//! non-zero, but a VM that wedges harder must not hang the suite either.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "PlainServerSocketContract";
const MARKER: &str = "PLAIN_SERVER_SOCKET_CONTRACT_OK";
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
    // The grouped spelling. The per-flag `CRATONVM_SYNTHETIC_NET_SOCKETS` still
    // works but prints a deprecation line on every run.
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
        .expect("failed to launch the plain-ServerSocket contract fixture");

    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        match child.try_wait().expect("failed to poll the fixture") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "{FIXTURE} (synthetic_sockets={synthetic_sockets}) did not finish within \
                     {RUN_TIMEOUT:?}; a ServerSocket operation is blocked"
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
fn plain_server_socket_matches_the_jdk_contract() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[plain_server_socket_contract] cratonvm binary not found; skipping");
        return;
    };
    for synthetic_sockets in [false, true] {
        run_fixture(&binary, synthetic_sockets);
    }
}
