// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A plain in-process `SSLServerSocket` echo must actually move bytes.
//!
//! `SSLServerSocket.accept()` used to record the accepted socket's TLS stream
//! id in two ways that both failed:
//!
//! * as a raw `set_field` on a `javax/net/ssl/SSLSocket` allocated with the
//!   REAL loaded class layout, whose field #2 is reference-typed — the
//!   field-layout guard silently drops a mismatched `Int` write there, exactly
//!   as `new13_finish_socket` already documented for the CLIENT path; and
//! * in the raw rustls id space rather than the `RUSTLS_SOCK_ID_BASE`-offset
//!   one the stream natives route on, so even a surviving write would have sent
//!   `s2_tls_write` to the native-tls registry, where an accepted rustls stream
//!   does not exist.
//!
//! `new13_resolve_tls_id` therefore answered -1 and every I/O method on the
//! accepted socket read that as "closed": the server half failed with
//! `SSLSocketOutputStream.write: stream is closed` before a byte moved, while
//! the identical Java passed on HotSpot.
//!
//! The fixture is end-to-end on purpose. Nothing narrower would have caught
//! this: the listener works, `accept()` returns a socket, the handshake
//! completes, and `getOutputStream()` hands back a stream — the object graph is
//! entirely plausible right up until the first write. Verified to FAIL on a
//! pre-fix binary and to pass on HotSpot before being committed.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Prerequisite gate: a MISSING binary skips, unless `CRATONVM_REQUIRE_E2E`
/// demands otherwise. See `common::require_binary`.
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

#[test]
fn accepted_ssl_server_socket_can_write_back_to_its_peer() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[sslserversocket_accept_echo] cratonvm binary not found; skipping");
        return;
    };
    let resources = workspace_root().join("vm/tests/resources");
    let certs = resources.join("cratonvm/tlscerts");
    // The fixture reads its identity from these; a missing one would surface as
    // an ECHO-FAIL that looks like the bug under test, so check it here where
    // the message can say what is actually wrong.
    for name in ["server.crt", "server.key", "ca.crt"] {
        assert!(
            certs.join(name).exists(),
            "test PEM {name} missing from {}",
            certs.display()
        );
    }

    let output = Command::new(&binary)
        // The real-socket path is what an SSLServerSocket runs on; the
        // synthetic one would not exercise the accept path at all.
        .env("CRATONVM_REAL", "aqs,net-sockets")
        .arg("-cp")
        .arg(&resources)
        .arg("cratonvm.SslServerSocketEcho")
        .arg(&certs)
        .output()
        .expect("failed to launch the SSLServerSocket echo fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Assert on the fixture's own terminal marker rather than the exit status:
    // a fixture that dies before printing must not be able to look like a pass.
    assert!(
        stdout.contains("ECHO-OK"),
        "accepted SSLServerSocket connection could not echo its peer.\n\
         Before the accept-path fix this failed with \
         `SSLSocketOutputStream.write: stream is closed` on the server side \
         (or a client-side connect timeout, whichever surfaced first).\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
