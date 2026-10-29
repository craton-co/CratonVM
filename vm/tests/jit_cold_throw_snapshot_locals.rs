// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A masked exception-construction arm is a compile-time view, not the
//! interpreter's continuation. Its omitted local reads must not make the
//! inlined deoptimization frame discard the receiver or exception arguments.
//! The unfixed release binary throws an NPE in this probe's checkReadableBytes0.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn cold_throw_deoptimization_keeps_original_bytecode_locals_in_both_modes() {
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
        .join("tests/jit_cold_throw_snapshot_locals_fixtures/ColdThrowLocalsProbe.java");
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
                .arg("ColdThrowLocalsProbe")
                .env("CRATONVM_DBG_DEOPT", "1")
                .env("CRATONVM_DBG_JITC", "1")
                .env("CRATONVM_BG_COMPILE", "0")
                .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                .env("CRATONVM_JIT_IR_INLINE", "1")
                .env("CRATONVM_JIT_IR_INLINE_COLD_ATHROW", "1")
                .env("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", "1")
                .env("CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done =
                common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(120));
            assert!(!done.timed_out, "probe timed out: {mode}, nojit={nojit}");
            let stdout = String::from_utf8_lossy(&done.output.stdout);
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                done.output.status.success() && stdout.contains("COLD-THROW-LOCALS-OK"),
                "{mode}, nojit={nojit}: {stdout}\n{stderr}"
            );
            if !nojit {
                assert!(
                    stderr
                        .lines()
                        .any(|line| line.contains("cold throw arm TRAPPED")
                            && (line.contains("ColdThrowLocalsProbe$Response.resetBuffer")
                                || line.contains("ColdThrowLocalsProbe$Buf.readByte"))),
                    "{mode}: probe did not compile a masked cold arm: {stderr}"
                );
            }
        }
    }
}
