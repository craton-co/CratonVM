// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

#[test]
fn atomic_integer_access_preserves_preadd_volatile_gc_and_contention() {
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
        .join("tests/jit_atomic_integer_access_fixtures/AtomicIntegerAccessProbe.java");
    let javac = home
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let compiled = Command::new(javac)
        .arg("-d")
        .arg(dir.path())
        .arg(source)
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/jit_atomic_integer_access_fixtures/AtomicProtectedOsrProbe.java"),
        )
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
        .arg("-cp")
        .arg(dir.path())
        .arg("AtomicIntegerAccessProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("ATOMIC-INTEGER-ACCESS-OK"));
    let osr_oracle =
        Command::new(
            &home
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" }),
        )
        .arg("-cp")
        .arg(dir.path())
        .arg("AtomicProtectedOsrProbe")
        .output()
        .expect("protected OSR oracle");
    assert!(
        osr_oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&osr_oracle.stderr)
    );
    let osr_expected = String::from_utf8_lossy(&osr_oracle.stdout).replace("\r\n", "\n");
    assert!(osr_expected.contains("ATOMIC-PROTECTED-OSR-OK"));
    for mode in ["--jdk-only", "--compatible"] {
        for (nojit, compact) in [(false, true), (false, false), (true, true)] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode);
            if nojit {
                cmd.arg("--nojit");
            }
            cmd.arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("AtomicIntegerAccessProbe")
                .env("CRATONVM_DBG", "jitc,jit-method-stats")
                // Use the production tiering door. The eager door can pin a
                // C1 increment before its first call, bypassing the invocation
                // counter and leaving this optimizing-tier check unexercised.
                .env("CRATONVM_BG_COMPILE", "1")
                .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                .env("CRATONVM_INTRINSIC_STATS", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if !compact {
                cmd.env("CRATONVM_COMPACT_REF_FIELDS", "0");
            }
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}/{nojit}: {stdout}\n{stderr}"
            );
            assert_eq!(stdout, expected, "{mode}/{nojit}: {stderr}");
            if !nojit {
                for method in ["increment", "read"] {
                    assert!(
                        stderr.lines().any(|line| {
                            line.contains(&format!(
                                "[ir] admission AtomicIntegerAccessProbe.{method}("
                            )) && line.contains("admitted to the optimizing pipeline")
                        }),
                        "{mode}/{compact}: missing optimizing {method}: {stderr}"
                    );
                }
            }
        }
    }
    for mode in ["--jdk-only", "--compatible"] {
        for gc in ["UseGenerationalGC", "UseG1GC", "UseZGC"] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg(format!("-XX:+{gc}"))
                .arg("--java-home")
                .arg(&home)
                .arg("-Xms64m")
                .arg("-Xmx128m")
                .arg("-cp")
                .arg(dir.path())
                .arg("AtomicProtectedOsrProbe")
                .env("CRATONVM_DBG", "jitc")
                .env("CRATONVM_BG_COMPILE", "1")
                .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done = common::wait_draining(
                cmd.spawn().expect("protected OSR"),
                Duration::from_secs(120),
            );
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}: {stdout}\n{stderr}"
            );
            assert_eq!(stdout, osr_expected, "{mode}: {stderr}");
            assert!(
                stderr.lines().any(|line| line
                    .contains("osr optimizing REUSE AtomicProtectedOsrProbe.protectedLoop")),
                "{mode}: no optimizing protected entry: {stderr}"
            );
        }
    }
}
