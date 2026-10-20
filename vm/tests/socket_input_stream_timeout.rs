// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for SO_TIMEOUT on all SocketInputStream read overloads.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE: &str = "SocketInputStreamTimeout";

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

fn run_probe(binary: &Path, nojit: bool, real_net_sockets: bool) {
    // Keep the peer outside the VM. This makes a zero-byte read unambiguously
    // a bug in the client socket path, rather than a side effect of a VM-side
    // test server closing its socket early.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind timeout probe peer");
    let port = listener
        .local_addr()
        .expect("timeout probe peer has no local address")
        .port();
    let peer = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("failed to make timeout probe peer nonblocking");
        // AN IDLE TIMEOUT, NOT A TOTAL BUDGET.
        //
        // This was a 5-second deadline on the WHOLE loop, started here — before
        // the VM process below has even been spawned. The loop's own mandatory
        // sleeps are 6 x 600 ms = 3.6 s of that budget, leaving under 1.4 s for
        // a debug-build VM to boot, load the fixture and make six connections.
        // On a shared host it does not fit: the peer returned 5 connections and
        // the test failed `left: 5, right: 6`, saying the fixture "did not
        // exercise every read overload" — an accusation against the VM for
        // being slow to start.
        //
        // What the bail-out is actually for is a VM that never connects at all,
        // and that question is answered by time since the LAST connection, not
        // by a clock that starts before the peer exists. Six slow-but-steady
        // connections now always complete, however long the boot took, while a
        // VM that dies or never dials still ends the thread in 30 s.
        const IDLE_BAILOUT: Duration = Duration::from_secs(30);
        let mut last_progress = Instant::now();
        let mut connections = 0;
        while connections < 6 && last_progress.elapsed() < IDLE_BAILOUT {
            match listener.accept() {
                Ok((_socket, _peer)) => {
                    connections += 1;
                    // The VM client has a 100 ms SO_TIMEOUT; this must keep
                    // the connection alive substantially longer than that.
                    thread::sleep(Duration::from_millis(600));
                    // AFTER the sleep: the sleep is work this peer chose to do,
                    // so charging it to the idle clock would put the deadline
                    // back in a race with the VM.
                    last_progress = Instant::now();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("timeout probe peer accept failed: {error}"),
            }
        }
        connections
    });
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
        .arg(port.to_string())
        .output()
        .expect("failed to launch SocketInputStream timeout fixture");
    let connections = peer.join().expect("timeout probe peer panicked");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("SOCKET_INPUT_STREAM_TIMEOUT_OK"),
        "{FIXTURE} failed (nojit={nojit}, real_net_sockets={real_net_sockets}). stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        connections, 6,
        "{FIXTURE} did not exercise every read overload before and after connect"
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
