// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An OSR-compiled loop whose virtual call is served by an inline PIC/MIC hit
//! arm must retract that call's shadow-stack publication on every iteration.
//! Since the cold-call service started publishing a fresh safepoint of its own
//! (`1f27937f66`), the hit arm's service consumed the site's pending shadow
//! reload at compile time; the shared `.done` reload then emitted nothing and
//! every call leaked its live homes until the 2 MiB shadow stack was full
//! (`[JIT] shadow-stack overflow: N push(es) bailed on the end guard`). See
//! `docs/feature-designs/deopt-osr.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn an_osr_loop_with_a_virtual_call_does_not_leak_shadow_pushes_in_both_modes() {
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
        .join("tests/r17_shadow_leak_osr_fixtures/ShadowLeakOsrProbe.java");
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
        for arm in ["b", "d"] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("ShadowLeakOsrProbe")
                .arg(arm)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done =
                common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(180));
            assert!(!done.timed_out, "probe timed out: {mode}, arm {arm}");
            let stdout = String::from_utf8_lossy(&done.output.stdout);
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                done.output.status.success() && stdout.contains("SHADOW-LEAK-OSR-OK"),
                "{mode}, arm {arm}: {stdout}\n{stderr}"
            );
            assert!(
                !stderr.contains("shadow-stack overflow"),
                "{mode}, arm {arm}: the OSR loop leaked shadow pushes:\n{stderr}"
            );
        }
    }
}
