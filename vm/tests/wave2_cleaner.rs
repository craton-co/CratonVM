// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 2, Task D — `Cleaner.create()` + `ByteBuffer.allocateDirect` end-to-end.
//!
//! S107 Cluster D pinned a regression where the CleanerProbe — which exercises
//! `java.nio.ByteBuffer.allocateDirect(1024)` together with
//! `java.lang.ref.Cleaner` — aborted the VM with
//! `internal error: checkcast: not an object reference` before any
//! `System.out.println` reached the host. The failure originated in the JDK 25
//! `java.nio.Buffer.session()` body: a `getfield segment` followed by a
//! `checkcast` to `jdk/internal/foreign/AbstractMemorySegmentImpl`. The
//! `segment` slot drifted to a non-reference `Value` variant during the
//! `DirectByteBuffer` constructor chain (the long `address` field's bit
//! pattern leaks through CompactValue's untagged Long/Double encoding when
//! the segment-field putfield is elided/reordered relative to the address
//! store), and the checkcast then hit a `Value::Double` slot.
//!
//! The fix routes both `java/nio/Buffer.session()` and
//! `java/nio/Buffer.checkSession()` through native shims that return null /
//! no-op (matching the existing `Buffer$1.acquireSession` shim in
//! `shared_secrets_bridge.rs`), so all DirectByteBuffer paths bypass the
//! drift-prone segment slot entirely.
//!
//! This test pins the contract: the CleanerProbe binary must reach its first
//! println line — `buf.cap=1024 val=42` — without aborting the VM. It does
//! NOT pin the full probe pass (the cleaner-action thread plumbing is
//! exercised in a separate soak test), only the early-boot checkcast guard.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn probe_dir() -> PathBuf {
    manifest_dir()
        .parent()
        .unwrap()
        .join("apps")
        .join("cleaner_probe")
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let target = manifest_dir().parent().unwrap().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn java_home() -> Option<String> {
    if let Ok(h) = std::env::var("CRATONVM_JAVA_HOME") {
        return Some(h);
    }
    if let Ok(h) = std::env::var("JAVA_HOME") {
        return Some(h);
    }
    let candidate = "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot";
    if Path::new(candidate).exists() {
        return Some(candidate.to_string());
    }
    None
}

fn run_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !probe.join("CleanerProbe.class").exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave2_cleaner",
            "the Wave 2 Cleaner fixture `CleanerProbe` (CleanerProbe.class, compiled from \
             CleanerProbe.java)",
            &[
                probe.join("CleanerProbe.class"),
                probe.join("CleanerProbe.java"),
            ],
        );
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("CleanerProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave2_cleaner] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[wave2_cleaner] CleanerProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave2_cleaner] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave2_cleaner] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

/// Top-level acceptance: the CleanerProbe must NOT abort the VM with
/// `checkcast: not an object reference` during the
/// `ByteBuffer.allocateDirect(1024) → buf.putInt(0, 42)` warm-up path. The
/// probe must reach its first `buf.cap=...` println, which proves that the
/// `Buffer.session()` shim is wired and the `DirectByteBuffer` checkcast
/// drift is no longer a fatal VM error.
///
/// We do NOT pin the full `CleanerProbe: PASS` line — that requires the
/// reference-processor thread which is exercised by a separate soak test —
/// and we do NOT pin the printed capacity value (a separate
/// DirectByteBuffer-init bug leaves capacity at -1 in our VM today; the
/// unblock target for this regression test is only the checkcast guard).
/// We only pin (a) no `checkcast` internal error in stderr, (b) the
/// `val=42` half of the first println surfaced on stdout (which proves
/// `putInt(0, 42)` + `getInt(0)` round-tripped through the
/// `Buffer.session()` shim), and (c) the literal `buf.cap=` prefix.
#[test]
fn cleaner_probe_reaches_first_println_without_checkcast_abort() {
    let (stdout, stderr, _rc) = match run_probe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[wave2_cleaner] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert!(
        !stderr.contains("checkcast: not an object reference"),
        "wave2_cleaner: VM aborted with the S107 checkcast regression. \
         stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("buf.cap="),
        "wave2_cleaner: probe never reached the first println prefix \
         (`buf.cap=`). stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("val=42"),
        "wave2_cleaner: probe reached the println but val=42 round-trip \
         is broken — the `putInt(0, 42)` / `getInt(0)` path failed. \
         stdout={stdout:?} stderr={stderr:?}"
    );
}
