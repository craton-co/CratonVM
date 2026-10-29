// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 10 (lane throws;
//! `docs/feature-designs/jdk-only-mode.md`):
//! a `fillInStackTrace()` refresh of a throwable built with suppression
//! disabled (`Throwable(String, Throwable, false, true)`) left
//! `suppressedExceptions` holding `SUPPRESSED_SENTINEL`, so every later
//! `addSuppressed` was recorded where HotSpot drops it
//! (`CRATONVM_THROWABLE_REFILL_KEEPS_SUPPRESSION`, default ON). Both modes must
//! print HotSpot's output; the `=0` arm is the control, reported, not
//! asserted (expected to differ on the `quiet-after-refill` lines).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

const SWITCH: &str = "CRATONVM_THROWABLE_REFILL_KEEPS_SUPPRESSION";

#[test]
fn a_refill_does_not_turn_disabled_suppression_back_on() {
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
        .join("tests/r18_throws_refill_keeps_suppression_fixtures/R18ThrowsSuppression.java");
    let compiled = Command::new(home.join("bin").join(tool("javac")))
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
    let oracle = Command::new(home.join("bin").join(tool("java")))
        .arg("-cp")
        .arg(dir.path())
        .arg("R18ThrowsSuppression")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(
        expected.contains("quiet-after-refill suppressed=0\n"),
        "{expected}"
    );
    assert!(
        expected.contains("loud-after-refill suppressed=2\n"),
        "{expected}"
    );
    for mode in ["--jdk-only", "--compatible"] {
        for arm in [None, Some("0")] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("R18ThrowsSuppression")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match arm {
                Some(word) => cmd.env(SWITCH, word),
                None => cmd.env_remove(SWITCH),
            };
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            match arm {
                None => {
                    assert!(
                        !done.timed_out && done.output.status.success(),
                        "{mode}/default: {stdout}\n{stderr}"
                    );
                    assert_eq!(stdout, expected, "{mode}: {stderr}");
                }
                Some(_) => {
                    // The control: the old mirror records the suppressed
                    // exception after the refill. Report-only -- under
                    // `--compatible` the old behaviour can also exit
                    // non-zero, so neither status nor output is asserted.
                    if done.timed_out || !done.output.status.success() {
                        eprintln!(
                            "{mode}/{SWITCH}=0 (control) did not exit cleanly (timed_out={}, status={:?}):\n{stdout}\n{stderr}",
                            done.timed_out, done.output.status
                        );
                    } else if stdout == expected {
                        eprintln!("{mode}/{SWITCH}=0 (control) printed HotSpot's output");
                    } else {
                        eprintln!("{mode}/{SWITCH}=0 (control) differs from HotSpot:\n{stdout}");
                    }
                }
            }
        }
    }
}
