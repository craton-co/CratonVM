// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 9 (lane osrpoly;
//! `docs/internal/fixed-bugs/r18-osr-door-opaque-entry-drops-the-polymorphic-splice-FIXED-20261006.md`):
//! the optimizing OSR door declines its IR attempt where the single-pass OSR
//! body splices a polymorphic in-loop site (`CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS`,
//! default ON). Both arms must print HotSpot's output; the default arm names
//! the decline, the `=0` arm never does.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

// The decline line. Since JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-4 /
// OSRFOLLOW18-4) the door asks it before any compile ("not attempted -- ...",
// `osr_optimizing_build_declined`); the in-compile "IR attempt declined -- ..."
// line it replaced is gone. Matched on the part both spellings shared.
const DECLINE: &str = "the single-pass OSR body splices the polymorphic site";

#[test]
fn osr_door_prefers_the_spliced_single_pass_body_for_a_polymorphic_loop() {
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
    let tool = |name: &str| {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
        .find(|p| p.join("bin").join(tool("javac")).is_file());
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/r18_osrpoly_prefer_spliced_single_pass_fixtures/OsrPolyProbe.java");
    let compiled = Command::new(home.join("bin").join(tool("javac")))
        .arg("-d")
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("javac");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let oracle = Command::new(home.join("bin").join(tool("java")))
        .arg("-cp")
        .arg(dir.path())
        .arg("OsrPolyProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("OSR-POLY-OK"));
    for mode in ["--jdk-only", "--compatible"] {
        for arm in [None, Some("0")] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("OsrPolyProbe")
                .env("CRATONVM_DBG", "jitc")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match arm {
                Some(word) => cmd.env("CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS", word),
                None => cmd.env_remove("CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS"),
            };
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}/{arm:?}: {stdout}\n{stderr}"
            );
            assert_eq!(stdout, expected, "{mode}/{arm:?}: {stderr}");
            let declined = stderr
                .lines()
                .any(|l| l.contains("OsrPolyProbe.poly") && l.contains(DECLINE));
            // The decline is asked only when the optimizing OSR door compiles
            // the loop; a run that never reached the door proves nothing.
            let door_reached = stderr
                .lines()
                .any(|l| l.contains("osr optimizing") && l.contains("OsrPolyProbe.poly"));
            match arm {
                None if door_reached => assert!(declined, "{mode}: no decline line: {stderr}"),
                Some(_) => assert!(!declined, "{mode}: =0 still declined: {stderr}"),
                None => {}
            }
        }
    }
}
