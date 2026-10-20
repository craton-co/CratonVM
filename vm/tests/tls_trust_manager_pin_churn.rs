// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! In-memory `SSLEngine` handshakes against a custom `X509TrustManager` on both
//! sides, while peer threads allocate, print HotSpot's output on G1 in both
//! modes: no `NoSuchMethodError: java.lang.Object.check*Trusted`.
//!
//! Under `--compatible` the handshake reaches `engine_consult_trust_managers`
//! (`native-builtins/src/t27_tls.rs`), which clones the context's TrustManagers
//! out of the rooted table as raw addresses. It used to pin them only after it
//! had allocated the certificate chain, the X509 mirrors and the authType
//! string, so on G1 a young collection in that window left the pins naming
//! vacated addresses: the first `invoke_virtual` read `ClassId(0)`
//! (`java.lang.Object`) and raised `NoSuchMethodError`, a `LinkageError` the
//! engine never turns into an alert. That is the stall netty's
//! `ParameterizedSslHandlerTest` kept hitting. Before the fix every launch of
//! this probe hit it within its first 25-80 handshakes; the probe runs 300.
//! See `docs/internal/fixed-suite-bugs/netty/parameterizedsslhandlertest-residual-stalls-FIXED-20261013.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str =
    include_str!("tls_trust_manager_pin_churn_fixtures/TlsTrustManagerChurnProbe.expected");

#[test]
fn trust_managers_are_pinned_before_the_handshake_allocates() {
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
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
                .map(|p| target.join(p).join(exe))
                .find(|p| p.is_file())
        });
    let Some(binary) = common::require_binary(binary.filter(|p| p.is_file())) else {
        return;
    };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
        .find(|p| {
            p.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
                .is_file()
        });
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/tls_trust_manager_pin_churn_fixtures/TlsTrustManagerChurnProbe.java");
    let compiled =
        Command::new(
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
        )
        .args(["-d"])
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    // `--compatible` is the mode whose handshake runs the VM's own TLS engine;
    // `--jdk-only` runs the JDK's, and must match too. G1 on a small heap is
    // what makes a young collection land inside the window.
    let configs: [(&str, &[&str]); 3] = [
        ("--compatible", &["-XX:+UseG1GC", "--Xmx", "600m"]),
        ("--compatible", &[]),
        ("--jdk-only", &["-XX:+UseG1GC", "--Xmx", "600m"]),
    ];
    for (mode, extra) in configs {
        let mut cmd = Command::new(&binary);
        cmd.arg(mode)
            .arg("--java-home")
            .arg(&home)
            .args(extra)
            .arg("-Dhandshakes=150")
            .arg("-cp")
            .arg(dir.path())
            .arg("TlsTrustManagerChurnProbe")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(600));
        assert!(!done.timed_out, "probe timed out: {mode} {extra:?}");
        let stdout = String::from_utf8_lossy(&done.output.stdout);
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            done.output.status.success() && stdout == EXPECTED,
            "{mode} {extra:?}: stdout differs from HotSpot's:
{stdout}
{stderr}"
        );
    }
}
