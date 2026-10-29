// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! FFM417-6 (j27 wave 4, lane FFMROWS): the stop-the-world vs blocked-downcall
//! deadlock of `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`
//! (fixed 2026-10-12) had only a probe, no test.
//!
//! `tools/probes/jit/ffm-stw/R16FfmDeadlockShape.java`: thread A downcalls into
//! C and blocks there (Linux `read` on a pipe, Windows `WaitForSingleObject`);
//! the main thread allocates, runs `System.gc()`, and only then signals A.
//! HotSpot runs A's downcall in `_thread_in_native`, so the collection does
//! not wait for A and the program prints `PASS`. A VM that counts the blocked
//! downcall as a running Java thread never ends: the pause waits for A, A
//! waits for the signal behind the pause. The bound below turns that hang into
//! a failure.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

/// HotSpot 25's lines (both platforms), in order.
const HOTSPOT_LINES: [&str; 4] = [
    "gc ran while A was blocked in C",
    "A state while blocked in C: RUNNABLE",
    "signalled true",
    "PASS",
];

fn javac_name() -> &'static str {
    if cfg!(windows) {
        "javac.exe"
    } else {
        "javac"
    }
}

fn find_binary() -> Option<PathBuf> {
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
    common::require_binary(binary.filter(|p| p.is_file()))
}

fn find_jdk() -> Option<PathBuf> {
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
        .find(|p| p.join("bin").join(javac_name()).is_file());
    common::require_jdk(home)
}

#[test]
fn blocked_downcall_does_not_stall_a_collection_in_either_mode_or_tier() {
    let Some(binary) = find_binary() else {
        return;
    };
    let Some(home) = find_jdk() else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tools/probes/jit/ffm-stw/R16FfmDeadlockShape.java");
    let compiled = Command::new(home.join("bin").join(javac_name()))
        .arg("-d")
        .arg(dir.path())
        .arg(&source)
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["--jdk-only", "--compatible"] {
        for nojit in [false, true] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode).arg("--enable-native-access=ALL-UNNAMED");
            if nojit {
                cmd.arg("--nojit");
            }
            cmd.arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("R16FfmDeadlockShape")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done = common::wait_draining(
                cmd.spawn().expect("launch probe"),
                Duration::from_secs(90),
            );
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out,
                "{mode} nojit={nojit}: the collection waited for the thread blocked in C \
                 (deadlock). stdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                done.output.status.success(),
                "{mode} nojit={nojit}: {stdout}\n{stderr}"
            );
            let lines: Vec<&str> = stdout
                .lines()
                .filter(|l| HOTSPOT_LINES.contains(l))
                .collect();
            assert_eq!(
                lines,
                HOTSPOT_LINES,
                "{mode} nojit={nojit}: not HotSpot's lines. stdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
    }
}
