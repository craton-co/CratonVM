// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A throw loop under a lambda body called through an interface reuses the
//! thread's capture memo.
//!
//! The memo was refused for every such capture: the `Thread.run` middle-frame
//! screen lets any lambda-body callee through, and a screen that engaged kept
//! the memo from being made even though the caller (`task.run()`, an
//! `invokeinterface`) is not the `run()V` call the rule needs. Every JUnit or
//! `ForkJoinPool` task frame is such a pair, so each `new Throwable()` of
//! Netty's paranoid leak tracking walked and laid out the whole stack, and
//! `AdaptiveByteBufAllocatorTest.purgeScanShouldEvictIdleChunks` took 51 s
//! against HotSpot's 19 s. See
//! `docs/feature-designs/jit-ir-optimizations.md`.
//!
//! The probe prints HotSpot's output in both modes, and the capture-memo census
//! (`CRATONVM_DBG_CAPTURE_MEMO=1`) must report the captures as hits.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const FRAMES: &str = "ThrowableMemoLambdaTaskProbe.lambda$main$0:31\n\
ThrowableMemoLambdaTaskProbe.work:11\n\
ThrowableMemoLambdaTaskProbe.descend:16\n";

/// `hits=<n>` of the last capture-memo census line.
fn memo_hits(stderr: &str) -> Option<u64> {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("[capture-memo] hits="))?;
    line["[capture-memo] hits=".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[test]
fn a_throw_loop_under_a_lambda_task_hits_the_capture_memo() {
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
        .join("tests/throwable_memo_lambda_task_fixtures/ThrowableMemoLambdaTaskProbe.java");
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
    let expected = format!(
        "{FRAMES}{}ThrowableMemoLambdaTaskProbe.main:36\ndepth=16 sink=200000\nLAMBDA-MEMO-OK\n",
        "ThrowableMemoLambdaTaskProbe.descend:18\n".repeat(12)
    );
    for mode in ["--real-jdk", "--compatible"] {
        let mut cmd = Command::new(&binary);
        cmd.arg(mode)
            .arg("--java-home")
            .arg(&home)
            .arg("-cp")
            .arg(dir.path())
            .arg("ThrowableMemoLambdaTaskProbe")
            .env("CRATONVM_DBG_CAPTURE_MEMO", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(180));
        assert!(!done.timed_out, "probe timed out: {mode}");
        let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            done.output.status.success(),
            "{mode}: the probe exited with {:?}:\n{stdout}\n{stderr}",
            done.output.status
        );
        assert!(
            stdout == expected,
            "{mode}: stdout differs from HotSpot's:\n{stdout}\n{stderr}"
        );
        // 200,000 captures. Before the fix every one was refused (11 hits in 8,000).
        let hits = memo_hits(&stderr)
            .unwrap_or_else(|| panic!("{mode}: no capture-memo census line:\n{stderr}"));
        assert!(
            hits >= 100_000,
            "{mode}: the captures did not reuse the memo: hits={hits}"
        );
    }
}
