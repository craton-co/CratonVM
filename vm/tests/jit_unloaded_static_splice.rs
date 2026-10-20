// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

#[test]
fn unloaded_static_splice_resumes_original_scopes_without_replaying_effects() {
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
        .join("tests/jit_unloaded_static_splice_fixtures/UnloadedStaticSpliceProbe.java");
    let javac = home
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let compiled = Command::new(javac)
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
        .arg("-cp")
        .arg(dir.path())
        .arg("UnloadedStaticSpliceProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("UNLOADED-STATIC-SPLICE-OK"));
    let osr_oracle =
        Command::new(
            &home
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" }),
        )
        .arg("-cp")
        .arg(dir.path())
        .arg("UnloadedStaticOsrProbe")
        .output()
        .expect("OSR HotSpot oracle");
    assert!(
        osr_oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&osr_oracle.stderr)
    );
    let osr_expected = String::from_utf8_lossy(&osr_oracle.stdout).replace("\r\n", "\n");
    for mode in ["--jdk-only", "--compatible"] {
        for nojit in [false, true] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode);
            if nojit {
                cmd.arg("--nojit");
            }
            cmd.arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("UnloadedStaticSpliceProbe")
                .env("CRATONVM_DBG", "jitc,jit-method-stats")
                .env("CRATONVM_BG_COMPILE", "0")
                .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}/{nojit}: {stdout}\n{stderr}"
            );
            assert_eq!(stdout, expected, "{mode}/{nojit}: {stderr}");
            let mut osr_cmd = Command::new(&binary);
            osr_cmd.arg(mode);
            if nojit {
                osr_cmd.arg("--nojit");
            }
            osr_cmd
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("UnloadedStaticOsrProbe")
                .env("CRATONVM_DBG", "jitc,jit-method-stats")
                .env("CRATONVM_BG_COMPILE", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let osr_done = common::wait_draining(
                osr_cmd.spawn().expect("OSR probe"),
                Duration::from_secs(120),
            );
            let osr_stdout = String::from_utf8_lossy(&osr_done.output.stdout).replace("\r\n", "\n");
            let osr_stderr = String::from_utf8_lossy(&osr_done.output.stderr);
            assert!(
                !osr_done.timed_out && osr_done.output.status.success(),
                "{mode}/{nojit}: {osr_stdout}\n{osr_stderr}"
            );
            assert_eq!(osr_stdout, osr_expected, "{mode}/{nojit}: {osr_stderr}");
            if !nojit {
                assert!(
                    osr_stderr
                        .lines()
                        .any(|line| line.contains("osr optimizing")
                            && line.contains("UnloadedStaticOsrProbe.main")
                            && line.contains("stub=true")),
                    "{mode}: no optimizing OSR entry: {osr_stderr}"
                );
                assert!(
                    osr_stderr
                        .lines()
                        .any(|line| line.contains("unloaded-static-call trap")
                            && line.contains("UnloadedStaticOsrProbe.main")),
                    "{mode}: no cold-call OSR exit: {osr_stderr}"
                );
            }
            if !nojit && mode == "--jdk-only" {
                assert!(
                    stderr
                        .lines()
                        .any(|line| line.contains("unloaded-static-call trap")
                            && (line.contains("UnloadedStaticSpliceProbe.step")
                                || line.contains("UnloadedStaticSpliceProbe.check"))),
                    "{mode}: the unloaded static call never became a precise exit: {stderr}"
                );
            }
        }
    }
    // The ordinary optimizing callee's cold service runs Java after the
    // single-pass caller has retracted its original call publication. Its
    // fresh String must remain readable across every System.gc in that path.
    for mode in ["--jdk-only", "--compatible"] {
        for gc in ["UseGenerationalGC", "UseG1GC", "UseZGC"] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg(format!("-XX:+{gc}"))
                .arg("-Xms64m")
                .arg("-Xmx256m")
                .arg("-cp")
                .arg(dir.path())
                .arg("UnloadedStaticSpliceProbe")
                .env("CRATONVM_DBG", "jitc,gc-stress=262144")
                .env("CRATONVM_BG_COMPILE", "1")
                .env("CRATONVM_DISABLE_DEFAULT_WATCHDOG", "1")
                .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done = common::wait_draining(
                cmd.spawn().expect("cold service GC probe"),
                Duration::from_secs(120),
            );
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}/{gc}: {stdout}\n{stderr}"
            );
            assert_eq!(stdout, expected, "{mode}/{gc}: {stderr}");
            assert!(
                stderr
                    .lines()
                    .any(|line| line
                        .contains("callee-compile UnloadedStaticSpliceProbe.loopAndFail")),
                "{mode}/{gc}: the ordinary optimizing callee was not exercised: {stderr}"
            );
        }
    }
}
