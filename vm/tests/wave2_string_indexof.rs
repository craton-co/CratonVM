// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 2, T19.H1 — `String.indexOf(String, int)` regression.
//!
//! The cglib `TypeUtils.map` regex path sits in a tight loop calling
//! `String.indexOf("[]", from)` and incrementing `from` by `idx + 1` until
//! `idx == -1`. JDK 25's compact-String layout dispatches the public 2-arg
//! method into the package-private static helper
//! `String.indexOf([BBILjava/lang/String;I)I`, which in turn calls
//! `StringLatin1.indexOf` / `StringUTF16.indexOf` /
//! `StringUTF16.indexOfLatin1`. None of those bytecode helpers are wired up
//! in cratonvm, so before this fix the loop never terminated and the T19.H1
//! 45 s watchdog tripped during cglib's `EmitUtils.<clinit>`.
//!
//! Pin the contract that:
//!   * `"hello world hello".indexOf("world")        == 6`   (no fromIndex)
//!   * `"hello world hello".indexOf("hello", 3)     == 12`  (with fromIndex)
//!   * the probe reaches the literal `OK` line.
//!
//! The probe (`apps/string_indexof_probe/SiProbe.java`) intentionally uses
//! only direct `String.indexOf` calls — no cglib, no regex, no scaffolding —
//! so a regression here points unambiguously at the indexOf surface in
//! `native-builtins/src/lang_string.rs`.

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
        .join("string_indexof_probe")
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
    if !probe.join("SiProbe.class").exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave2_string_indexof",
            "the Wave 2 String.indexOf fixture `SiProbe` (SiProbe.class, compiled from \
             SiProbe.java)",
            &[probe.join("SiProbe.class"), probe.join("SiProbe.java")],
        );
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("SiProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave2_string_indexof] failed to spawn cratonvm: {e}");
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
                    panic!(
                        "[wave2_string_indexof] SiProbe timed out after {:?} \
                         — String.indexOf likely regressed back into the \
                         StringLatin1/StringUTF16 fall-through loop",
                        timeout
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave2_string_indexof] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave2_string_indexof] wait_with_output failed: {e}");
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
fn si_probe_index_of_string_and_from_match_hotspot() {
    let (stdout, stderr, rc) = match run_probe(Duration::from_secs(30)) {
        Some(o) => o,
        None => {
            eprintln!("[wave2_string_indexof] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "wave2_string_indexof: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("idx=6"),
        "wave2_string_indexof: expected `idx=6` (\"hello world hello\".indexOf(\"world\")). \
         Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("idx2=12"),
        "wave2_string_indexof: expected `idx2=12` \
         (\"hello world hello\".indexOf(\"hello\", 3)). \
         Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("OK"),
        "wave2_string_indexof: SiProbe must reach the final `OK` line. \
         Got stdout={:?}",
        stdout
    );
}
