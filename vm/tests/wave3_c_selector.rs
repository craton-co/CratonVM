// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 3, Task C — `apps/selector_probe/SelectorProbe.java` regression test.
//!
//! Pin the JDK NIO selector loop end-to-end against the cratonvm CLI in
//! real-JDK mode. The probe sets up a `ServerSocketChannel` + `Selector`
//! pair, drives `select(500)` until OP_ACCEPT fires, accepts the
//! connection, exchanges a single byte, and exits.
//!
//! Required output lines (`STEP 5` of the Wave 3.C method):
//!   * `server.port=NNN` — non-blocking bind on an OS-chosen port worked.
//!   * `server.accepted=true` — `Selector.select(...)` reported OP_ACCEPT
//!     and `ServerSocketChannel.accept()` returned a non-null SocketChannel.
//!   * `server.recv=42` — the accepted socket read the byte the client wrote.
//!   * `OK` — full happy-path completed without throwing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn worktree_root() -> PathBuf {
    manifest_dir().parent().unwrap().to_path_buf()
}

fn probe_dir() -> PathBuf {
    worktree_root().join("apps").join("selector_probe")
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
    let target = worktree_root().join("target");
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
    // Workspace shared target — when the worktree shares the parent
    // workspace's target dir.
    let shared = worktree_root()
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("target"));
    if let Some(t) = shared {
        for profile in &["release", "debug"] {
            let candidate = t.join(profile).join(exe);
            if candidate.exists() {
                return Some(candidate);
            }
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

fn run_selector_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !probe.join("SelectorProbe.class").exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave3-c",
            "the Wave 3 Task C fixture `SelectorProbe` (SelectorProbe.class, compiled from \
             SelectorProbe.java)",
            &[
                probe.join("SelectorProbe.class"),
                probe.join("SelectorProbe.java"),
            ],
        );
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("SelectorProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave3-c] failed to spawn cratonvm: {e}");
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
                    panic!("[wave3-c] SelectorProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave3-c] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave3-c] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

#[test]
fn selector_probe_loopback_echo() {
    let (stdout, stderr, rc) = match run_selector_probe(Duration::from_secs(30)) {
        Some(o) => o,
        None => {
            eprintln!("[wave3-c] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "wave3-c: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    assert!(
        stdout.lines().any(|l| l.starts_with("server.port=")),
        "wave3-c: SelectorProbe must report `server.port=NNN`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("server.accepted=true"),
        "wave3-c: SelectorProbe must report `server.accepted=true`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("server.recv=42"),
        "wave3-c: SelectorProbe must echo the byte (`server.recv=42`). Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("OK"),
        "wave3-c: SelectorProbe must reach the final `OK` line. Got stdout={:?}",
        stdout
    );
}
