// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An exception thrown out of callees the optimizing tier spliced into an
//! OSR-compiled loop must reach the `catch` that covers the call. The OSR
//! exception route judged the stashed inlined chain by its innermost scope (the
//! spliced callee), took it for a foreign frame and let the exception escape the
//! method: Netty's `NioEventLoopTest.testTaskRemovalOnShutdown...` saw `null`
//! where it expected a `RejectedExecutionException`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn an_exception_out_of_spliced_callees_is_caught_by_the_osr_loop_in_both_modes() {
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
        .join("tests/jit_osr_inlined_throw_caught_fixtures/OsrInlinedThrowProbe.java");
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
                .arg("OsrInlinedThrowProbe")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done =
                common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(120));
            assert!(!done.timed_out, "probe timed out: {mode}, nojit={nojit}");
            let stdout = String::from_utf8_lossy(&done.output.stdout);
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                done.output.status.success() && stdout.contains("OSR-INLINED-THROW-OK"),
                "{mode}, nojit={nojit}: {stdout}\n{stderr}"
            );
        }
    }
}
