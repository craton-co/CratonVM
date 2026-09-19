// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RBIGDEC.1 — full BigInteger/BigDecimal arithmetic round-trip on real JDK.
//!
//! Pin the full output of `probes/BdProbe.java` (restored 2026-08-07; it used
//! to live under the gitignored `apps/`, which made this whole file a silent
//! `ok` in 0.00 s — see the header of `rbigdec1_arithmetic.rs`):
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

/// Tag used in every diagnostic this file emits.
const TAG: &str = "rbigdec1-full";

fn workspace_root() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn probe_dir() -> PathBuf {
    workspace_root().join("apps").join("bigdecimal_probe")
}

/// Locate the checked-in probe source. See the twin comment in
/// `rbigdec1_arithmetic.rs`: `apps/` is gitignored, so `probes/BdProbe.java` is
/// the durable home and the `apps/` path is kept only for local stagings.
fn probe_source() -> [PathBuf; 3] {
    [
        probe_dir().join("BdProbe.java"),
        workspace_root().join("probes").join("BdProbe.java"),
        workspace_root()
            .join("tools")
            .join("probes")
            .join("BdProbe.java"),
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

/// Prefer a JDK-relative `javac` over whatever is on `PATH`.
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

/// Compile `BdProbe.java` into [`probe_classes_dir`]. See the twin in
/// `rbigdec1_arithmetic.rs` for the full rationale on which conditions skip and
/// which fail.
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
        "[{TAG}] javac reported success but {} is absent — the probe's class name changed.",
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
