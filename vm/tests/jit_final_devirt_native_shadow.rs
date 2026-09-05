// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `final`-method devirtualiser must refuse a body a registered native
//! shadows.
//!
//! `final` promises no subclass declares another BODY. It promises nothing
//! about CratonVM's native registry, which shadows a JDK method by registering
//! on a class name — usually a SUBCLASS of the one that declares the body.
//! `invoke_or_native` honours that by walking the RECEIVER's superclass chain;
//! a devirtualised site has thrown the receiver away and calls the classfile
//! body directly. So the interpreter and compiled code answered the same call
//! differently, and only after tier-up.
//!
//! Two `public final` methods on `java.nio.channels.spi
//! .AbstractInterruptibleChannel` carried it into netty, measured 2026-09-05 on
//! a release binary:
//!
//!  * `isOpen()` is `return !closed`, and CratonVM's channel closes cleared
//!    their own side table without ever writing that field — 397,739 OPEN
//!    answers in 400,000 calls on a closed `SocketChannel`, first at call
//!    2,261.
//!  * `close()` opens with `synchronized (closeLock)`, a field the datagram
//!    factory never seeded — 3,487 `NullPointerException`s in 4,000 closes,
//!    first at call 512, each leaking a UDP socket.
//!
//! netty's `AbstractChannel.close()` runs `doClose0()` and then, in the same
//! `finally`, `outboundBuffer.close(cause)`, which throws
//! `IllegalStateException: close() must be invoked after the channel is
//! closed.` on a channel still reading open. `DnsNameResolverTest` logged it
//! 384 times with the JIT on and **0** times under `--nojit`. The retirement
//! page is `channeloutboundbuffer-close-ordering-was-final-devirt-FIXED-20260905`
//! (cited without a directory prefix on purpose: it lives in the internal tree,
//! which is stripped from public history, and a path a public checkout cannot
//! resolve is worse than a name it can search for).
//!
//! # Why both arms run
//!
//! `--nojit` is the interpreter's answer and is the one this VM was already
//! giving. The default arm is the compiled one. The defect is precisely that
//! the two disagreed, so a test that ran only one of them could not see it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

const FIXTURE: &str = "FinalDevirtNativeShadowProbe";
const MARKER: &str = "FINAL_DEVIRT_NATIVE_SHADOW_OK";

/// A release binary runs the probe in a few seconds; a debug one is slower and
/// a loaded host slower still.
const TIMEOUT: Duration = Duration::from_secs(600);

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("jit_final_devirt_native_shadow_fixtures")
}

fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let path = PathBuf::from(bin);
        if path.exists() {
            return Some(path);
        }
    }
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .join("target");
    ["release", "debug"]
        .into_iter()
        .map(|profile| target.join(profile).join(exe))
        .find(|path| path.exists())
}

fn java_home() -> Option<PathBuf> {
    let java = if cfg!(windows) { "java.exe" } else { "java" };
    for variable in ["CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(variable) {
            let home = PathBuf::from(home);
            if home.join("bin").join(java).exists() {
                return Some(home);
            }
        }
    }
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot");
    default.join("bin").join(java).exists().then_some(default)
}

/// Compile the fixture if the checked-in `.class` is missing.
///
/// A `javac` that cannot be LAUNCHED is the one legitimate skip. A `javac` that
/// ran and rejected the source is a broken fixture and must fail loudly.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join(format!("{FIXTURE}.class"));
    if class_file.exists() {
        return true;
    }
    let source = dir.join(format!("{FIXTURE}.java"));
    if !source.exists() {
        return false;
    }
    let javac = java_home()
        .map(|home| {
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
        })
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("javac"));
    match Command::new(javac).arg("-d").arg(&dir).arg(&source).output() {
        Err(_) => false,
        Ok(out) => {
            assert!(
                out.status.success(),
                "[jit_final_devirt_native_shadow] the checked-in fixture failed to compile \
                 — fix the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            class_file.exists()
        }
    }
}

/// `None` means a prerequisite was missing (and `CRATONVM_REQUIRE_E2E` was not
/// set); `common::require_binary` decides whether that is a skip or a failure.
fn run_probe(nojit: bool) -> Option<String> {
    if !ensure_probe_compiled() {
        eprintln!("[jit_final_devirt_native_shadow] fixture .class unavailable; skipping");
        return None;
    }
    let bin = cratonvm_binary()?;
    let Some(home) = java_home() else {
        eprintln!("[jit_final_devirt_native_shadow] JDK not found; skipping");
        return None;
    };
    let mut command = Command::new(&bin);
    if nojit {
        command.arg("--nojit");
    }
    // The screen is default-on. Set it explicitly so a stray
    // `CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN=0` in the environment cannot
    // turn this test into a vacuous pass by disabling the JIT path it exists
    // to check — the same trap `CRATONVM_BIN` sprang on the vm suite.
    command.env("CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN", "1");
    command.env("CRATONVM_JIT_FINAL_DEVIRT", "1");
    let mut child = match command
        .arg("--java-home")
        .arg(&home)
        .arg("-cp")
        .arg(probe_dir())
        .arg(FIXTURE)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[jit_final_devirt_native_shadow] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[jit_final_devirt_native_shadow] probe timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[jit_final_devirt_native_shadow] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "[jit_final_devirt_native_shadow] cratonvm exited {:?} (nojit={nojit})\n\
         --- stdout ---\n{stdout}\n--- stderr (tail) ---\n{}",
        out.status.code(),
        stderr.lines().rev().take(40).collect::<Vec<_>>().join("\n"),
    );
    Some(stdout)
}

#[test]
fn compiled_and_interpreted_channel_state_agree_after_close() {
    for nojit in [false, true] {
        let Some(stdout) = run_probe(nojit) else {
            return;
        };
        // Every row must be present, not just the marker: a fixture that threw
        // before reaching a row would otherwise pass on the rows it did reach.
        for row in [
            "SocketChannel.isOpen after close",
            "ServerSocketChannel.isOpen after close",
            "DatagramChannel.isOpen after close",
            "SocketChannel.isOpen while open",
            "DatagramChannel.close threw",
            "DatagramChannel still open after close",
        ] {
            assert!(
                stdout.contains(row),
                "[jit_final_devirt_native_shadow] row {row:?} missing (nojit={nojit}); \
                 the probe did not run to completion\n--- stdout ---\n{stdout}"
            );
        }
        assert!(
            stdout.contains(MARKER),
            "[jit_final_devirt_native_shadow] the probe reported a divergence (nojit={nojit})\n\
             --- stdout ---\n{stdout}"
        );
    }
}
