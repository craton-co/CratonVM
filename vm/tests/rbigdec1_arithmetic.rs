// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RBIGDEC.1 — `apps/bigdecimal_probe/BdProbe.java` regression test.
//!
//! Pin the round-trip of:
//!   * `BigDecimal.ONE.add(BigDecimal.TEN)` → "11"
//!   * `BigInteger.TWO.multiply(BigInteger.TEN)` → "20"
//!   * `OK`
//!
//! against the cratonvm CLI in real-JDK mode.
//!
//! Background: `java/math/BigInteger.<clinit>` and
//! `java/math/BigDecimal.<clinit>` historically silent-swallowed in real-JDK
//! mode, leaving the static constants `null` (the KC16 cascade NPE Session
//! 95 surfaced as "Cannot read field 'signum' because the object is null").
//! `vm/src/vm/vm_util.rs::post_clinit_fixup` now populates the constants
//! and applies a descriptor-cache poison so the synthetic-stub-shaped
//! `bi_read`/`bd_read` natives in `native-builtins/src/lib.rs` see the
//! decimal-string overlay at slot 0 instead of coercing it back to
//! `Value::Int(ptr_low)` via the real-JDK `signum:I` descriptor.
//!
//! KNOWN PARTIAL: with the current iteration the BigInteger arithmetic
//! still surfaces as `Object@<hash>` (the natives' `signum` slot mismatch
//! cannot be fully patched from `vm_util.rs` alone — the natives in
//! `lib.rs` would need a layout-aware refactor). The test asserts the
//! string `"OK"` is reached (proving the `<clinit>` cascade no longer
//! NPEs and the program runs to completion); the `"11"`/`"20"` line
//! checks are gated on the eventual full fix landing in the natives.

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
        eprintln!("[rbigdec1] BdProbe.class missing — run javac in apps/bigdecimal_probe");
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("BdProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rbigdec1] failed to spawn cratonvm: {e}");
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
                    panic!("[rbigdec1] BdProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[rbigdec1] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[rbigdec1] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

/// BdProbe must:
///   1. exit with rc == 0
///   2. print the literal "OK" line — proves the KC16 cascade NPE on
///      BigInteger/BigDecimal `<clinit>` is fully recovered
///
/// Stretch goal (target output `11\n20\nOK`): currently a known partial.
/// This test asserts the recovery, not the arithmetic round-trip — the
/// arithmetic gate is parked behind the synthetic-stub-vs-real-JDK
/// native-layout work that lives in `native-builtins/src/lib.rs`.
#[test]
fn bdprobe_runs_to_ok_without_npe() {
    let (stdout, stderr, rc) = match run_bdprobe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[rbigdec1] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "rbigdec1: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("OK"),
        "rbigdec1: BdProbe must reach the final `OK` line. Got stdout={:?}",
        stdout
    );
    // Ensure the BigDecimal silent-swallow line either is absent or fully
    // recovered (the `Post-clinit fixup: BigDecimal ZERO/ONE/TWO/TEN
    // populated (4/4)` warn is acceptable; an NPE traceback is not).
    assert!(
        !stdout.contains("Cannot read field 'signum' because the object is null"),
        "rbigdec1: BdProbe must not surface the BigDecimal/BigInteger \
         cascade NPE. Got stdout={:?}",
        stdout
    );
}

/// Full arithmetic round-trip — used to be `#[ignore]`d while the natives
/// in `native-builtins/src/lib.rs` (`bi_read`/`bd_read`) read the
/// synthetic-stub slot layout instead of the real-JDK one.  The Session
/// closing RBIGDEC.1 refactored those natives via
/// `NativeContext::resolve_field_index`, so the gate is now live in CI.
#[test]
fn bdprobe_arithmetic_roundtrip() {
    let (stdout, stderr, rc) = match run_bdprobe(Duration::from_secs(60)) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(rc, Some(0), "rc={:?} stderr={:?}", rc, stderr);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.starts_with('[')).collect();
    assert!(
        lines.iter().any(|l| l.trim() == "11"),
        "expected '11' line: {:?}",
        lines
    );
    assert!(
        lines.iter().any(|l| l.trim() == "20"),
        "expected '20' line: {:?}",
        lines
    );
    assert!(
        lines.iter().any(|l| l.trim() == "OK"),
        "expected 'OK' line: {:?}",
        lines
    );
}
