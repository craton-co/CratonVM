// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for the wildcard `InetAddress` that `new ServerSocket(0)`
//! binds through, on a COLD VM under GC stress.
//!
//! Retires `serversocket-bind-null-inetaddress-net-sockets-20260803`: under
//! `CRATONVM_REAL=net-sockets` the real JDK bytecode drives `sun/nio/ch/Net`,
//! and `Net.bind`'s first act is `addr.isLinkLocalAddress()`. Two cross-call
//! GC-safety defects (see the fixture's own doc comment) made that `addr`
//! null, so the ordinary `new ServerSocket(0)` died with
//! `NullPointerException: … because "addr" is null`.
//!
//! **`stress` is what reproduces it, not `moving-young`** — `moving_young` is
//! DEFAULT-ON (`CRATONVM_NO_MOVING_YOUNG` opts out), so the collector already
//! relocates; `stress=65536` is only what makes it collect often enough to
//! land inside the unrooted window. Both arms below run with `stress`, and the
//! second one additionally spells `moving-young` out so the flag translation
//! itself stays covered.
//!
//! The fixture asserts everything itself and passes on HotSpot; this harness
//! runs it and enforces a deadline, because a VM that regresses harder than
//! the tracked NPE could hang instead of failing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "WildcardBindUnderGcStress";
const MARKER: &str = "WILDCARD_BIND_UNDER_GC_STRESS_OK";
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
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
    ["release", "debug"]
        .into_iter()
        .map(|profile| workspace_root().join("target").join(profile).join(exe))
        .find(|path| path.exists())
}

fn classpath_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources");
    dir.join("cratonvm")
        .join(format!("{FIXTURE}.class"))
        .exists()
        .then_some(dir)
}

/// `gc_tokens` is the whole value of the grouped `CRATONVM_GC` variable: a
/// second assignment REPLACES the first rather than adding to it, so every
/// token a case needs has to be in one string.
/// An empty `gc_tokens` means "do not set `CRATONVM_GC` at all" — an empty
/// grouped value is not the same thing as an absent one, and only the absent
/// form is the shipped default this is meant to exercise.
fn run_fixture(binary: &Path, classpath: &Path, gc_tokens: &str) {
    let mut command = Command::new(binary);
    if gc_tokens.is_empty() {
        command.env_remove("CRATONVM_GC");
    } else {
        command.env("CRATONVM_GC", gc_tokens);
    }
    let mut child = command
        // `--nojit` keeps the failing path interpreted, which is where the
        // defect lived and where the fixture's cold-start ordering is
        // deterministic.
        .arg("--nojit")
        .arg("--Xmx")
        .arg("128m")
        .arg("-c")
        .arg(classpath)
        .arg(format!("cratonvm.{FIXTURE}"))
        .env("CRATONVM_REAL", "net-sockets")
        .env("CRATONVM_THREADS", "-default-watchdog")
        .env_remove("CRATONVM_GC_STRESS")
        .env_remove("CRATONVM_MOVING_YOUNG")
        .env_remove("CRATONVM_REAL_NET_SOCKETS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("[{FIXTURE}] failed to spawn {binary:?}: {e}"));

    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        match child.try_wait().expect("failed to poll the fixture") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("[{FIXTURE}] CRATONVM_GC={gc_tokens} did not finish within {RUN_TIMEOUT:?}");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    let output = child
        .wait_with_output()
        .expect("failed to collect the fixture output");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert!(
        output.status.success() && stdout.contains(MARKER),
        "[{FIXTURE}] CRATONVM_GC={gc_tokens} failed.\nstatus={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
}

#[test]
fn wildcard_bind_survives_a_cold_start_under_gc_stress() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[{FIXTURE}] cratonvm binary not found; skipping");
        return;
    };
    let Some(classpath) = classpath_dir() else {
        eprintln!("[{FIXTURE}] {FIXTURE}.class not compiled; skipping");
        return;
    };
    for gc_tokens in ["stress=65536", "stress=65536,moving-young"] {
        run_fixture(&binary, &classpath, gc_tokens);
    }
}

/// The same contract with NO GC stress. If this ever fails while the stressed
/// test passes, the fix has been replaced by something that only happens to
/// work when collections are frequent.
#[test]
fn wildcard_bind_is_correct_without_gc_stress() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[{FIXTURE}] cratonvm binary not found; skipping");
        return;
    };
    let Some(classpath) = classpath_dir() else {
        eprintln!("[{FIXTURE}] {FIXTURE}.class not compiled; skipping");
        return;
    };
    run_fixture(&binary, &classpath, "");
}
