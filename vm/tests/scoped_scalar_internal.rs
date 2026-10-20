// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

#[test]
fn scoped_scalar_internal_preserves_hotspot_carriers_sessions_and_collectors() {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
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
        .join("tests/scoped_wide_access_fixtures/ScopedScalarInternalProbe.java");
    let javac = home
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let compiled = Command::new(javac)
        .args([
            "--add-exports",
            "java.base/jdk.internal.misc=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.foreign=ALL-UNNAMED",
        ])
        .arg("-d")
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let java = home
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" });
    let oracle = Command::new(java)
        .args([
            "--add-exports",
            "java.base/jdk.internal.misc=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.foreign=ALL-UNNAMED",
            "-Dscoped.rounds=20",
        ])
        .arg("-cp")
        .arg(dir.path())
        .arg("ScopedScalarInternalProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("SCOPED-SCALAR-INTERNAL-OK"));
    for collector in ["-XX:+UseGenerationalGC", "-XX:+UseG1GC", "-XX:+UseZGC"] {
        for mode in ["--jdk-only", "--compatible"] {
            for nojit in [false, true] {
                let mut cmd = Command::new(&binary);
                cmd.arg(mode);
                if nojit {
                    // Warmup only drives compilation; every semantic case still runs.
                    cmd.args(["--nojit", "-Dscoped.rounds=20"]);
                }
                cmd.arg(collector)
                    .arg("-Xmx256m")
                    .args([
                        "--add-exports",
                        "java.base/jdk.internal.misc=ALL-UNNAMED",
                        "--add-exports",
                        "java.base/jdk.internal.foreign=ALL-UNNAMED",
                    ])
                    .arg("--java-home")
                    .arg(&home)
                    .arg("-cp")
                    .arg(dir.path())
                    .arg("ScopedScalarInternalProbe")
                    .env("CRATONVM_DBG", "jit-method-stats")
                    .env("CRATONVM_GC_STRESS", "65536")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let done =
                    common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
                let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
                let stderr = String::from_utf8_lossy(&done.output.stderr);
                assert!(
                    !done.timed_out && done.output.status.success(),
                    "{collector}/{mode}/{nojit}: {stdout}\n{stderr}"
                );
                assert_eq!(stdout, expected, "{collector}/{mode}/{nojit}: {stderr}");
                if !nojit {
                    let served = stderr
                        .lines()
                        .find(|line| line.contains("JIT ScopedMemoryAccess direct:"))
                        .and_then(|line| line.split("served=").nth(1))
                        .and_then(|tail| tail.split_whitespace().next())
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0);
                    assert!(
                        served > 1000,
                        "{collector}/{mode}: the scalar Internal helpers never served an access: {stderr}"
                    );
                }
            }
        }
    }
}
