// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for the resolved wildcard contract of
//! `new InetSocketAddress(port)`.

use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = "InetSocketAddressPortOnly";

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
    let executable = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    ["release", "debug"]
        .into_iter()
        .map(|profile| {
            workspace_root()
                .join("target")
                .join(profile)
                .join(executable)
        })
        .find(|path| path.exists())
}

fn java_home() -> Option<PathBuf> {
    for variable in ["CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(variable) {
            let home = PathBuf::from(home);
            if home
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" })
                .exists()
            {
                return Some(home);
            }
        }
    }
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot");
    default
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" })
        .exists()
        .then_some(default)
}

fn classpath_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(workspace_root().join("vm/tests/resources"));
    candidates.into_iter().find(|dir| {
        dir.join("cratonvm")
            .join(format!("{FIXTURE}.class"))
            .exists()
    })
}

#[test]
fn port_only_inet_socket_address_is_resolved_in_jit_and_interpreter() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[inet_socket_address_port_only] cratonvm binary not found; skipping");
        return;
    };
    let Some(java_home) = java_home() else {
        eprintln!("[inet_socket_address_port_only] JDK not found; skipping");
        return;
    };
    let Some(classpath) = classpath_dir() else {
        eprintln!("[inet_socket_address_port_only] fixture class not compiled; skipping");
        return;
    };

    for nojit in [false, true] {
        let mut command = Command::new(&binary);
        if nojit {
            command.arg("--nojit");
        }
        let output = command
            .arg("--java-home")
            .arg(&java_home)
            .arg("-c")
            .arg(&classpath)
            .arg(format!("cratonvm.{FIXTURE}"))
            .output()
            .unwrap_or_else(|error| panic!("failed to launch {binary:?}: {error}"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("INET_SOCKET_ADDRESS_PORT_ONLY_OK"),
            "{FIXTURE} failed (nojit={nojit}). stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
