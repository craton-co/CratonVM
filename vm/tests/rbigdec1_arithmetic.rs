// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RBIGDEC.1 — `probes/BdProbe.java` regression test.
//!
//! # This test was VACUOUS until 2026-08-07
//!
//! The fixture it drives was `apps/bigdecimal_probe/BdProbe.java`, and `apps/`
//! is `.gitignore`d (line 12) — so the file was never tracked and was absent
//! from every checkout. `run_bdprobe` found no `BdProbe.class`, returned `None`,
//! both tests below returned early, and cargo reported `ok` in 0.00 s. Nothing
//! here could fail.
//!
//! The fixture is restored at `probes/BdProbe.java` (`probes/` IS tracked) and
//! is now compiled on demand into `target/bd-probe-classes/<tag>/`. A missing
//! fixture reports loudly and fails under `CRATONVM_REQUIRE_E2E` — see
//! `common::require_fixture`.
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

/// Tag used in every diagnostic this file emits.
const TAG: &str = "rbigdec1";

fn workspace_root() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn probe_dir() -> PathBuf {
    workspace_root().join("apps").join("bigdecimal_probe")
}

/// Locate the checked-in probe source.
///
/// `apps/` is gitignored (.gitignore line 12), which is how
/// `apps/bigdecimal_probe/BdProbe.java` stayed missing from the tree long
/// enough for both RBIGDEC.1 harnesses to report `ok` in 0.00 s while
/// asserting nothing. The fixture now lives at `probes/BdProbe.java` —
/// `probes/` is tracked — and the `apps/` path is kept only so an existing
/// local staging still wins.
fn probe_source() -> [PathBuf; 2] {
    [
        probe_dir().join("BdProbe.java"),
        workspace_root().join("probes").join("BdProbe.java"),
    ]
}

/// Per-test-binary output directory under `target/`, so the two RBIGDEC.1
/// harnesses never write the same class files concurrently.
fn probe_classes_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join("bd-probe-classes")
        .join(TAG)
}

/// Prefer a JDK-relative `javac` over whatever is on `PATH`, so the probe is
/// compiled by the same toolchain the run below uses.
fn javac_path() -> PathBuf {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    for var in ["CRATONVM_TEST_JDK", "CRATONVM_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(var) {
            let candidate = PathBuf::from(home).join("bin").join(exe);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("javac")
}

/// Compile `BdProbe.java` into [`probe_classes_dir`]. Returns the classes
/// directory, or `None` when a prerequisite is genuinely absent.
///
/// A missing FIXTURE is reported through `common::require_fixture` (loud, and a
/// failure under `CRATONVM_REQUIRE_E2E`). A `javac` that cannot be LAUNCHED, or
/// that is too old for `--release 21`, is the one legitimate skip. A javac that
/// RAN and REJECTED the source is a hard failure — see `probe_compile_guard.rs`.
fn ensure_probe_compiled() -> Option<PathBuf> {
    let candidates = probe_source();
    let src = common::require_fixture(
        TAG,
        "the RBIGDEC.1 fixture `BdProbe.java` (must print `11`, `20`, `OK` and exit 0)",
        &candidates,
    )?;
    let classes = probe_classes_dir();
    let main_class = classes.join("BdProbe.class");
    if main_class.exists() {
        // Recompile whenever the source is newer than the class, so an edit to
        // the fixture is never masked by a stale artefact.
        let fresh = match (main_class.metadata(), src.metadata()) {
            (Ok(c), Ok(s)) => match (c.modified(), s.modified()) {
                (Ok(c), Ok(s)) => c >= s,
                _ => false,
            },
            _ => false,
        };
        if fresh {
            return Some(classes);
        }
    }
    let _ = std::fs::create_dir_all(&classes);
    let javac = javac_path();
    let out = match Command::new(&javac)
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            if common::require_e2e() {
                panic!(
                    "[{TAG}] {} is set, but `{}` could not be executed ({e}), so this test would \
                     have skipped and still reported `ok`.",
                    common::REQUIRE_VAR,
                    javac.display()
                );
            }
            eprintln!(
                "[{TAG}] `{}` could not be executed ({e}); skipping.",
                javac.display()
            );
            return None;
        }
    };
    if !out.status.success() {
        // javac REJECTED THE ARGUMENTS, not the source: an unsupported
        // `--release` means this javac never opened the file. That is a
        // missing-toolchain condition, not a broken probe.
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            if common::require_e2e() {
                panic!(
                    "[{TAG}] {} is set, but javac cannot target --release 21 ({}), so this test \
                     would have skipped and still reported `ok`.",
                    common::REQUIRE_VAR,
                    stderr_probe.lines().next().unwrap_or("").trim()
                );
            }
            eprintln!(
                "[{TAG}] javac cannot target --release 21 ({}); skipping.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success(),
        "[{TAG}] the checked-in probe fixture {} failed to compile — fix the .java source. javac \
         stderr:\n{}",
        src.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        main_class.exists(),
        "[{TAG}] javac reported success but {} is absent — the probe's class name changed; both \
         RBIGDEC.1 harnesses run the class `BdProbe`.",
        main_class.display()
    );
    Some(classes)
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
    // Compiles `probes/BdProbe.java` on demand; reports a MISSING fixture
    // loudly and fails under CRATONVM_REQUIRE_E2E.
    let probe = ensure_probe_compiled()?;
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
