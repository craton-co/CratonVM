// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RBIGDEC.1 — full BigInteger/BigDecimal arithmetic round-trip on real JDK.
//!
//! Pin the full output of `apps/bigdecimal_probe/BdProbe.java`:
//!     11
//!     20
//!     OK
//!
//! This is the strict version of the recovery test in
//! `rbigdec1_arithmetic.rs::bdprobe_runs_to_ok_without_npe` — it asserts the
//! arithmetic actually produces the expected decimal strings, not just that
//! the program exits cleanly.  It became un-ignorable once the natives in
//! `native-builtins/src/lib.rs` (`bi_read`/`bd_read`/`bi_alloc`/`bd_alloc`)
//! were taught the real-JDK slot layout (`signum:I` + `mag:[I` for
//! BigInteger; `intVal:BigInteger`/`scale:I`/`precision:I`/`intCompact:J`
//! for BigDecimal) via `NativeContext::resolve_field_index`.

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
        .join("bigdecimal_probe")
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

fn run_bdprobe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !probe.join("BdProbe.class").exists() {
        eprintln!("[rbigdec1-full] BdProbe.class missing — run javac in apps/bigdecimal_probe");
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("BdProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[rbigdec1-full] BdProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

#[test]
fn bdprobe_full_arithmetic_roundtrip() {
    let (stdout, stderr, rc) = match run_bdprobe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[rbigdec1-full] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "rbigdec1-full: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    // Ignore [tracing]-prefixed warn lines so the assertion only sees the
    // application stdout.  HotSpot prints "11\n20\nOK".
    let lines: Vec<&str> = stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.starts_with('[') && !l.is_empty())
        .collect();
    assert!(
        lines.iter().any(|l| *l == "11"),
        "rbigdec1-full: expected '11' line (BigDecimal.ONE.add(TEN)). Got lines={:?}\nstderr={:?}",
        lines,
        stderr
    );
    assert!(
        lines.iter().any(|l| *l == "20"),
        "rbigdec1-full: expected '20' line (BigInteger.TWO.multiply(TEN)). Got lines={:?}\nstderr={:?}",
        lines, stderr
    );
    assert!(
        lines.iter().any(|l| *l == "OK"),
        "rbigdec1-full: expected 'OK' line. Got lines={:?}",
        lines
    );
}
