// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: real-JDK `Instant` factories must execute their JDK bytecode.
//!
//! The synthetic `Instant` bridge remains available for classes genuinely
//! loaded as synthetic stubs.  It must not replace `Instant.now`,
//! `ofEpochSecond`, or `ofEpochMilli` in real-JDK mode: those factories create
//! real objects whose `toString()` is the ISO-8601 representation consumed by
//! Jackson and Spring Boot actuator endpoint serialization.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_SRC: &str = r#"
import java.time.Instant;

public class InstantRealJdkFactoryProbe {
  private static void check(String label, Instant value, String expected) {
    String actual = value.toString();
    if (!expected.equals(actual)) {
      throw new AssertionError(label + " expected=" + expected + " actual=" + actual);
    }
    System.out.println(label + "=" + actual);
  }

  public static void main(String[] args) {
    check("epoch", Instant.ofEpochSecond(1_700_000_000L), "2023-11-14T22:13:20Z");
    check("epochNano", Instant.ofEpochSecond(1_700_000_000L, 123_456_789L), "2023-11-14T22:13:20.123456789Z");
    check("milli", Instant.ofEpochMilli(1_700_000_000_123L), "2023-11-14T22:13:20.123Z");
    String now = Instant.now().toString();
    if (!now.endsWith("Z") || now.startsWith("Instant(")) {
      throw new AssertionError("now must be ISO-8601, actual=" + now);
    }
    System.out.println("now=" + now);
    System.out.println("OK");
  }
}
"#;

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
    None
}

/// Prerequisite gate: the lookup below is unchanged — only a MISSING JDK is
/// reported differently. See `common::require_jdk`.
fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in ["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(value) = std::env::var(var) {
            let path = PathBuf::from(value);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-instant-real-jdk-factory-probe");
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join("InstantRealJdkFactoryProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("InstantRealJdkFactoryProbe.class"));
    std::fs::write(&source, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&source)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[instant_real_jdk_factory] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[instant_real_jdk_factory_tostring] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("InstantRealJdkFactoryProbe.class").exists(),
        "[instant_real_jdk_factory] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(binary: &Path, jdk: &Path, classes: &Path, no_jit: bool) {
    let mut command = Command::new(binary);
    command.arg("--java-home").arg(jdk);
    if no_jit {
        command.arg("--nojit");
    }
    let output = command
        .arg("-c")
        .arg(classes)
        .arg("InstantRealJdkFactoryProbe")
        .output()
        .expect("run InstantRealJdkFactoryProbe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("OK"),
        "Instant factory probe failed (no_jit={no_jit}): {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    for expected in [
        "epoch=2023-11-14T22:13:20Z",
        "epochNano=2023-11-14T22:13:20.123456789Z",
        "milli=2023-11-14T22:13:20.123Z",
    ] {
        assert!(stdout.contains(expected), "missing `{expected}`:\n{stdout}");
    }
}

#[test]
fn real_jdk_instant_factories_preserve_iso_tostring_in_jit_and_interpreter() {
    let binary = match cratonvm_binary() {
        Some(binary) => binary,
        None => {
            eprintln!("[instant_real_jdk_factory_tostring] CRATONVM_BIN is not set; skipping");
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(jdk) => jdk,
        None => {
            eprintln!("[instant_real_jdk_factory_tostring] CRATONVM_TEST_JDK or JAVA_HOME is not set; skipping");
            return;
        }
    };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let classes = match compile_probe(&javac) {
        Some(classes) => classes,
        None => {
            eprintln!("[instant_real_jdk_factory_tostring] javac unavailable; skipping");
            return;
        }
    };

    run_probe(&binary, &jdk, &classes, false);
    run_probe(&binary, &jdk, &classes, true);
}
