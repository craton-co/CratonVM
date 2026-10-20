// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

#[test]
fn primitive_arraycopy_overlap_and_disjoint_ranges_preserve_hotspot() {
    check_arraycopy(false);
}

#[test]
fn ir_arraycopy_preserves_primitive_copies_and_authoritative_refusals() {
    check_arraycopy(true);
}

fn check_arraycopy(ir: bool) {
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
        .join("tests/jit_primitive_varhandle_reads_fixtures/ArraycopyOverlapProbe.java");
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
        .arg("ArraycopyOverlapProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("ARRAYCOPY-OVERLAP-OK"));
    for collector in ["-XX:+UseGenerationalGC", "-XX:+UseG1GC", "-XX:+UseZGC"] {
        for mode in ["--jdk-only", "--compatible"] {
            for nojit in [false, true] {
                let mut cmd = Command::new(&binary);
                cmd.arg(mode);
                if nojit {
                    cmd.arg("--nojit");
                }
                cmd.arg(collector)
                    .arg("-Xmx256m")
                    .arg("--java-home")
                    .arg(&home)
                    .arg("-cp")
                    .arg(dir.path())
                    .arg("ArraycopyOverlapProbe")
                    .env("CRATONVM_DBG", "jit-method-stats")
                    .env("CRATONVM_DBG_JITC", "1")
                    .env("CRATONVM_GC_STRESS", "65536")
                    .env("CRATONVM_C2_SUPERSEDE", if ir { "1" } else { "0" })
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if ir && !nojit {
                    cmd.env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                        .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                        .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                        .env("CRATONVM_DBG_IR_COMPILES", "1")
                        .env("CRATONVM_DBG_JIT_DISASM", "ArraycopyOverlapProbe.copyAndSum")
                        .env_remove("CRATONVM_JIT_IR_OVER_INTRINSIC");
                }
                let done =
                    common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
                let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
                let stderr = String::from_utf8_lossy(&done.output.stderr);
                assert!(
                    !done.timed_out && done.output.status.success(),
                    "{collector}/{mode}/nojit={nojit}: {stdout}\n{stderr}"
                );
                assert_eq!(stdout, expected, "{collector}/{mode}/nojit={nojit}: {stderr}");
                if ir && !nojit {
                    assert!(stderr.contains("[cratonvm-jit-disasm] full/ir ArraycopyOverlapProbe.copyAndSum("),
                        "{collector}/{mode}: no actual IR copy artifact: {stderr}");
                    assert!(stderr.contains("primitive-arraycopy direct served"),
                        "{collector}/{mode}: no direct primitive copy service: {stderr}");
                }

            }
        }
    }
}
