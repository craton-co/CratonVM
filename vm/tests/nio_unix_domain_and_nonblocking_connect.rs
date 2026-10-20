// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The real JDK's NIO channels under `--jdk-only`: a non-blocking connect is
//! in progress and the selector reports `OP_CONNECT` for it, Unix-domain
//! channels (`sun.nio.ch.UnixDomainSockets`) open, bind, connect and echo,
//! and the extended socket options answer on a channel with no connection yet.
//! The expected output is HotSpot 25's, line for line (Linux only).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str = "nb connect immediate=false
pending=true connected=false
select=1 ready=8
finishConnect=true connected=true
refused connect immediate=false
refused: java.net.ConnectException: Connection refused (connect failed)
uds bound=true name=s.sock
uds client echo=hello-uds n=9
uds server read=9 peerNamed=false
missing path: java.net.SocketException: No such file or directory
sc supports TCP_NODELAY=true
sc TCP_QUICKACK default=true
sc SO_INCOMING_NAPI_ID default=0
ssc TCP_QUICKACK default=true
ssc SO_INCOMING_NAPI_ID default=0
NIO-CONNECT-PROBE-DONE
";

#[test]
fn nonblocking_connect_unix_domain_and_extended_options_match_hotspot() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let binary = std::env::var_os("CRATONVM_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            ["release", "debug"]
                .into_iter()
                .map(|p| target.join(p).join("cratonvm"))
                .find(|p| p.is_file())
        });
    let Some(binary) = common::require_binary(binary.filter(|p| p.is_file())) else {
        return;
    };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([PathBuf::from("/home/victor/jdk25")])
        .find(|p| p.join("bin").join("javac").is_file());
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/nio_unix_domain_and_nonblocking_connect_fixtures/NioConnectProbe.java");
    let compiled = Command::new(home.join("bin").join("javac"))
        .args(["-d"])
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for nojit in [false, true] {
        let mut cmd = Command::new(&binary);
        cmd.arg("--jdk-only");
        if nojit {
            cmd.arg("--nojit");
        }
        cmd.arg("--java-home")
            .arg(&home)
            .arg("-cp")
            .arg(dir.path())
            .arg("NioConnectProbe")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(120));
        assert!(!done.timed_out, "probe timed out: nojit={nojit}");
        let stdout = String::from_utf8_lossy(&done.output.stdout);
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert_eq!(stdout, EXPECTED, "nojit={nojit}\n{stderr}");
    }
}
