// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A callee that calls `AtomicInteger.getAndIncrement()` keeps its own compile
//! instead of being spliced into its caller.
//!
//! The atomic ladders turn the call into one `LOCK XADD` at the compiled
//! method's own sites; a spliced body's calls are ordinary calls, so splicing
//! the wrapper copied `Unsafe.getAndAddInt`'s CAS retry loop into the loop. Once
//! `CRATONVM_JIT_IR_INLINE_COLD_ATHROW` let a wrapper with a cold `athrow` arm be
//! spliced, Netty's `new FastThreadLocal<Boolean>()` in a loop went from 8-9 ns to
//! 135-142 ns per iteration and `FastThreadLocalTest.testConstructionWithIndex`
//! (two billion of them, 120 s deadline) timed out. See
//! `docs/feature-designs/jdk-only-mode.md`.
//!
//! The probe prints HotSpot's output in both modes; under `CRATONVM_DBG=jitc` the
//! optimizing tier must also say it refused the wrapper.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str = "index=5 next=6\nsum=6000000\ncaught too many indexed variables next=2147483639\nATOMIC-WRAPPER-OK\n";

#[test]
fn a_callee_with_an_atomic_rmw_call_is_not_spliced_and_the_probe_matches_hotspot() {
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
        .join("tests/atomic_wrapper_splice_keeps_xadd_fixtures/AtomicWrapperSpliceProbe.java");
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
        let mut cmd = Command::new(&binary);
        cmd.arg(mode)
            .arg("--java-home")
            .arg(&home)
            .arg("-cp")
            .arg(dir.path())
            .arg("AtomicWrapperSpliceProbe")
            .env("CRATONVM_DBG", "jitc")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(180));
        assert!(!done.timed_out, "probe timed out: {mode}");
        // JIT r22 wave 7 (lane inline): `println` ends a line with the
        // platform separator, `\r\n` on Windows, on HotSpot as on this VM, so
        // the exact compare against `EXPECTED` (written with `\n`) failed on
        // every Windows run although the lines were HotSpot's (the log showed
        // identical text). Compare lines, as the other probe tests here do
        // (`replace("\r\n", "\n")`), and report the exit status on its own so a
        // crash after correct output is not reported as a stdout mismatch.
        let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            done.output.status.success(),
            "{mode}: the probe exited with {:?}:\n{stdout}\n{stderr}",
            done.output.status
        );
        assert!(
            stdout == EXPECTED,
            "{mode}: stdout differs from HotSpot's:\n{stdout}\n{stderr}"
        );
        assert!(
            !stderr.lines().any(|l| l.contains(
                "nest-static java/util/concurrent/atomic/AtomicInteger.getAndIncrement()I"
            ) && l.ends_with("SPLICED")),
            "{mode}: a callee's atomic call was spliced as the JDK's CAS loop:\n{stderr}"
        );
        if mode == "--jdk-only" && stderr.contains("bg-compile AtomicWrapperSpliceProbe") {
            assert!(
                stderr.contains("AtomicWrapperSpliceProbe.nextIndex()I")
                    && stderr.contains("atomic-intrinsic-callee-keeps-its-own-fast-path"),
                "{mode}: the optimizing tier did not refuse the wrapper that calls getAndIncrement:\n{stderr}"
            );
        }
    }
}
