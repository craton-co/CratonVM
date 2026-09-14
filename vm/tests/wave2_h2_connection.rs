// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 2 — H2 JDBC connection regression.
//!
//! Pins the contract that `DriverManager.getConnection("jdbc:h2:mem:...",
//! "sa", "")` succeeds end-to-end: driver-class load, ConnectionInfo
//! parsing, SessionLocal init (which transitively reads `sun/util/
//! calendar/ZoneInfoFile.<clinit>` -> `loadTZDB` from
//! `<javaHome>/lib/tzdb.dat`), schema DDL, INSERT, and SELECT.
//!
//! The bug this guards against: the synthetic `BufferedInputStream`
//! native overrides in `native-io/src/lib.rs` stored fields by slot index
//! using a stale 4-field layout (in/buf/pos/count) that does not match
//! the JDK 25 BIS instance layout (initialSize/buf/count/pos/markpos/
//! marklimit on top of `in` from FilterInputStream). The result was
//! `buf == null` after `<init>`, `markpos == 0` instead of `-1`, and
//! `BIS.read()` returning `-1` immediately. That EOF then caused
//! `DataInputStream(BIS(FIS(tzdb.dat))).readByte()` to fall into the
//! "File format not recognised" branch of `ZoneInfoFile.load`, throwing
//! `StreamCorruptedException` out of `JdbcConnection.<init>` as a
//! `SQLException("GeneralError")`.
//!
//! The fix dropped the BIS overrides so the real JDK 25 bytecode runs
//! end-to-end (it lazily allocates `buf` via `Unsafe.compareAndSetReference`,
//! which is already implemented). This test pins:
//!   * H2Test's `count=2` line (proves CREATE/INSERT/SELECT roundtrip)
//!   * The literal `H2Test: PASS` (final marker)
//!   * Exit code 0 (no swallowed/raised exceptions on the boot path)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn h2_dir() -> PathBuf {
    manifest_dir().parent().unwrap().join("apps").join("h2")
}

fn h2_jar() -> PathBuf {
    h2_dir().join("lib").join("h2-2.2.224.jar")
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

#[test]
fn h2test_connect_and_select_roundtrip() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[wave2_h2] skipping: cratonvm binary not built");
            return;
        }
    };
    let probe = h2_dir();
    // Loud, and a failure under CRATONVM_REQUIRE_E2E — see `common::require_fixture`.
    if !probe.join("H2Test.class").exists() {
        let _ = common::require_fixture(
            "wave2_h2",
            "the Wave 2 H2 fixture `H2Test` (H2Test.class, compiled from H2Test.java)",
            &[probe.join("H2Test.class"), probe.join("H2Test.java")],
        );
        return;
    }
    if !h2_jar().exists() {
        // NOTE: `.gitignore` line 14 is `**/*.jar`, so this jar can never be
        // committed — it is a genuine download prerequisite.
        let _ = common::require_fixture(
            "wave2_h2",
            "the H2 database jar `h2-2.2.224.jar` (a DOWNLOAD prerequisite: `**/*.jar` is \
             gitignored, so it is staged, never committed)",
            &[h2_jar()],
        );
        return;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    let cp = format!("{};{}", probe.display(), h2_jar().display());
    cmd.arg("-c").arg(&cp).arg("H2Test");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave2_h2] failed to spawn cratonvm: {e}");
            return;
        }
    };
    let timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[wave2_h2] H2Test timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[wave2_h2] try_wait failed: {e}");
                return;
            }
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "wave2_h2: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        out.status.code(),
        stdout,
        stderr,
    );
    assert!(
        stdout.contains("count=2"),
        "wave2_h2: expected `count=2`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("H2Test: PASS"),
        "wave2_h2: expected `H2Test: PASS`. Got stdout={:?}",
        stdout
    );
}
