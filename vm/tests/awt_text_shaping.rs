// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(feature = "awt")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn text_shaping_matches_hotspot_in_both_modes() {
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
    let javac_name = if cfg!(windows) { "javac.exe" } else { "javac" };
    let java_name = if cfg!(windows) { "java.exe" } else { "java" };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .find(|p| p.join("bin").join(javac_name).is_file());
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/font-shaping");
    let dir = tempfile::tempdir().expect("probe directory");
    let compiled = Command::new(home.join("bin").join(javac_name))
        .arg("-d")
        .arg(dir.path())
        .arg(fixture.join("CiFontShapeProbe.java"))
        .output()
        .expect("run javac");
    assert!(
        compiled.status.success(),
        "CiFontShapeProbe.java failed to compile: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let font = fixture.join("shaping.ttf");
    let oracle = Command::new(home.join("bin").join(java_name))
        .args(["-Djava.awt.headless=true", "-cp"])
        .arg(dir.path())
        .arg("CiFontShapeProbe")
        .arg(&font)
        .output()
        .expect("run HotSpot");
    assert!(
        oracle.status.success(),
        "HotSpot: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert_eq!(
        expected.lines().count(),
        10,
        "HotSpot must shape every fixture"
    );
    for mode in ["--real-jdk", "--compatible"] {
        let mut cmd = Command::new(&binary);
        cmd.arg(mode)
            .arg("--java-home")
            .arg(&home)
            .arg("-Djava.awt.headless=true")
            .arg("-cp")
            .arg(dir.path())
            .arg("CiFontShapeProbe")
            .arg(&font)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let done =
            common::wait_draining(cmd.spawn().expect("run CratonVM"), Duration::from_secs(120));
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            !done.timed_out && done.output.status.success(),
            "{mode}: timed_out={} status={}\n{stderr}",
            done.timed_out,
            done.output.status
        );
        assert_eq!(
            String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n"),
            expected,
            "{mode}: {stderr}"
        );
    }
}
