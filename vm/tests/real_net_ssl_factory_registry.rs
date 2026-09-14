// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The real-network gate must discard only legacy TLS stubs.  P68's bridge
//! methods remain responsible for constructing layout-correct SSL sockets.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

#[test]
fn real_net_sockets_keep_ssl_factory_bridge_without_synthetic_stubs() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[real_net_ssl_factory_registry] cratonvm binary not found; skipping");
        return;
    };
    let dump = std::env::temp_dir().join(format!(
        "cratonvm-real-net-ssl-factory-{}-{}.json",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let output = Command::new(binary)
        .env("CRATONVM_REAL_NET_SOCKETS", "1")
        .arg("--dump-native-registry")
        .arg(&dump)
        .arg("-c")
        .arg(workspace_root().join("vm/tests/resources"))
        .arg("cratonvm.RealNetSockets")
        .output()
        .expect("failed to launch real-network socket probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("REAL_NET_SOCKETS_OK"),
        "real-network socket probe failed. stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let census: serde_json::Value = serde_json::from_slice(
        &fs::read(&dump).expect("real-network socket probe did not write a native registry census"),
    )
    .expect("native registry census is not valid JSON");
    let entries: Vec<&serde_json::Value> = census["natives"]
        .as_array()
        .expect("native registry census has no natives array")
        .iter()
        .filter(|entry| entry["class"] == "javax/net/ssl/SSLSocketFactory")
        .collect();
    assert!(
        !entries.is_empty(),
        "SSLSocketFactory has no registered bridge methods"
    );
    assert!(
        entries.iter().all(|entry| entry["kind"] == "bridge"),
        "real-network SSLSocketFactory registry retained a non-bridge native: {entries:?}"
    );
    for descriptor in [
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
        "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
    ] {
        assert!(
            entries.iter().any(|entry| {
                entry["name"] == "createSocket" && entry["descriptor"] == descriptor
            }),
            "real-network SSLSocketFactory registry lost the createSocket{descriptor} bridge: {entries:?}"
        );
    }

    let server_entries: Vec<&serde_json::Value> = census["natives"]
        .as_array()
        .expect("native registry census has no natives array")
        .iter()
        .filter(|entry| entry["class"] == "javax/net/ssl/SSLServerSocketFactory")
        .collect();
    for descriptor in [
        "(I)Ljava/net/ServerSocket;",
        "(II)Ljava/net/ServerSocket;",
        "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
    ] {
        assert!(
            server_entries.iter().any(|entry| {
                entry["name"] == "createServerSocket" && entry["descriptor"] == descriptor
            }),
            "real-network SSLServerSocketFactory registry lost the createServerSocket{descriptor} bridge: {server_entries:?}"
        );
    }
    fs::remove_file(dump).expect("failed to remove native registry census");
}
