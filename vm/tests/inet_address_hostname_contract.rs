// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for the `hostName` an `InetAddress` mirror remembers.
//!
//! `InetAddress.toString()` is
//! `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`, so
//! whether a name was stored is directly observable. CratonVM used to store one
//! unconditionally — every mirror came from `alloc_inet_address(host, ip)` with
//! both fields set — so a literal-derived address printed
//! `127.0.0.1/127.0.0.1` where HotSpot prints `/127.0.0.1`.
//!
//! The fixture asserts the whole contract itself and passes on HotSpot; this
//! harness runs it in BOTH socket modes, because the two modes reach the
//! address-building code through different producers (`sun/nio/ch/Net` and the
//! `net_phase_e` RE.1/RE.2 surface), and a fix applied to only one of them
//! would look complete from either arm alone.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "InetAddressHostNameContract";
const MARKER: &str = "INETADDRESS_HOSTNAME_CONTRACT_OK";
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

fn classpath_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources");
    dir.join("cratonvm")
        .join(format!("{FIXTURE}.class"))
        .exists()
        .then_some(dir)
}

fn run_fixture(binary: &Path, classpath: &Path, synthetic_sockets: bool) {
    let mut command = Command::new(binary);
    // `real_net_sockets` is DEFAULT-ON. Selecting the synthetic surface takes
    // an explicit `CRATONVM_REAL=-net-sockets`; merely UNSETTING the variable
    // does not select it and would silently test the default twice.
    if synthetic_sockets {
        command.env("CRATONVM_REAL", "-net-sockets");
    } else {
        command.env_remove("CRATONVM_REAL");
    }
    let mut child = command
        .env_remove("CRATONVM_SYNTHETIC_NET_SOCKETS")
        .env_remove("CRATONVM_REAL_NET_SOCKETS")
        .arg("-c")
        .arg(classpath)
        .arg(format!("cratonvm.{FIXTURE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("[{FIXTURE}] failed to spawn {binary:?}: {e}"));

    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        match child.try_wait().expect("failed to poll the fixture") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("[{FIXTURE}] synthetic_sockets={synthetic_sockets} timed out");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    let output = child
        .wait_with_output()
        .expect("failed to collect the fixture output");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert!(
        output.status.success() && stdout.contains(MARKER),
        "[{FIXTURE}] synthetic_sockets={synthetic_sockets} failed.\nstatus={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
}

#[test]
fn inet_address_remembers_a_hostname_only_when_one_was_supplied() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[{FIXTURE}] cratonvm binary not found; skipping");
        return;
    };
    let Some(classpath) = classpath_dir() else {
        eprintln!("[{FIXTURE}] {FIXTURE}.class not compiled; skipping");
        return;
    };
    for synthetic_sockets in [false, true] {
        run_fixture(&binary, &classpath, synthetic_sockets);
    }
}
