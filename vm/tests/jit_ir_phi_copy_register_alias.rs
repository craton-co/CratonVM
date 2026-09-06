// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phi copies at a merge are a PARALLEL assignment in registers too.
//!
//! The optimizing tier sequentialises them over frame words
//! (`resolve_parallel_copy`) and then executes them over register homes, and
//! two distinct frame words can share a register: the allocator sees a phi's
//! live range as starting after the merge and a dying source's as ending at
//! it, so they do not interfere by its reckoning. They do interfere across the
//! copy sequence — an earlier copy's publish overwrites a register a later copy
//! still has to read — and the word-level order says nothing about it.
//!
//! Measured on a release binary, 2026-09-05, before the fix
//! (`jit_ir_phi_copy_register_alias_fixtures/PhiCopyRegisterAliasProbe.java`):
//!
//! ```text
//!   shape             wrong=37000/40000  first=3000
//!   Duration.toNanos  wrong=198356/200000 first=1644
//! ```
//!
//! and, in the wild, `ChronoUnit.MILLIS.getDuration().toNanos()` answering `0`,
//! which makes `LocalDateTime.truncatedTo(MILLIS)` throw
//! `ArithmeticException: / by zero` out of `LocalTime.truncatedTo` — two
//! failures in hibernate-reactive's `BasicTypesAndCallbacksForAllDBsTest`, and
//! the reason this is an end-to-end test rather than a unit test on the
//! resolver: the resolver was right, and only the layer under it was wrong.
//!
//! # Why both arms run
//!
//! `--nojit` is the interpreter's answer, and it was always correct. The defect
//! is that compiled code disagreed with it, so a test that ran one arm could
//! not see it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

const FIXTURE: &str = "PhiCopyRegisterAliasProbe";
const MARKER: &str = "PHI_COPY_REGISTER_ALIAS_OK";

/// A release binary runs the probe in a few seconds; a debug one is slower and
/// a loaded host slower still.
const TIMEOUT: Duration = Duration::from_secs(600);

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("jit_ir_phi_copy_register_alias_fixtures")
}

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
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .join("target");
    ["release", "debug"]
        .into_iter()
        .map(|profile| target.join(profile).join(exe))
        .find(|path| path.exists())
}

fn java_home() -> Option<PathBuf> {
    let java = if cfg!(windows) { "java.exe" } else { "java" };
    for variable in ["CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(variable) {
            let home = PathBuf::from(home);
            if home.join("bin").join(java).exists() {
                return Some(home);
            }
        }
    }
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot");
    default.join("bin").join(java).exists().then_some(default)
}

/// Compile the fixture if the checked-in `.class` is missing.
///
/// A `javac` that cannot be LAUNCHED is the one legitimate skip. A `javac` that
/// ran and rejected the source is a broken fixture and must fail loudly.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join(format!("{FIXTURE}.class"));
    if class_file.exists() {
        return true;
    }
    let source = dir.join(format!("{FIXTURE}.java"));
    if !source.exists() {
        return false;
    }
    let javac = java_home()
        .map(|home| {
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
        })
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("javac"));
    match Command::new(javac).arg("-d").arg(&dir).arg(&source).output() {
        Err(_) => false,
        Ok(out) => {
            assert!(
                out.status.success(),
                "[jit_ir_phi_copy_register_alias] the checked-in fixture failed to compile \
                 — fix the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            class_file.exists()
        }
    }
}

/// `None` means a prerequisite was missing (and `CRATONVM_REQUIRE_E2E` was not
/// set); `common::require_binary` decides whether that is a skip or a failure.
fn run_probe(nojit: bool) -> Option<String> {
    if !ensure_probe_compiled() {
        eprintln!("[jit_ir_phi_copy_register_alias] fixture .class unavailable; skipping");
        return None;
    }
    let bin = cratonvm_binary()?;
    let Some(home) = java_home() else {
        eprintln!("[jit_ir_phi_copy_register_alias] JDK not found; skipping");
        return None;
    };
    let mut command = Command::new(&bin);
    if nojit {
        command.arg("--nojit");
    }
    // Both are default-on. Set them explicitly so a stray `=0` in the
    // environment cannot turn this into a vacuous pass by disabling the very
    // layer it exists to check — each of them, on its own, makes the defect
    // disappear.
    command.env("CRATONVM_JIT_IR_PHI_COPY_REGS", "1");
    command.env("CRATONVM_JIT_IR_PHI_RESIDENCY", "1");
    let mut child = match command
        .arg("--java-home")
        .arg(&home)
        .arg("-cp")
        .arg(probe_dir())
        .arg(FIXTURE)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[jit_ir_phi_copy_register_alias] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[jit_ir_phi_copy_register_alias] probe timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[jit_ir_phi_copy_register_alias] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "[jit_ir_phi_copy_register_alias] cratonvm exited {:?} (nojit={nojit})\n\
         --- stdout ---\n{stdout}\n--- stderr (tail) ---\n{}",
        out.status.code(),
        stderr.lines().rev().take(40).collect::<Vec<_>>().join("\n"),
    );
    Some(stdout)
}

#[test]
fn compiled_and_interpreted_phi_merges_agree() {
    for nojit in [false, true] {
        let Some(stdout) = run_probe(nojit) else {
            return;
        };
        // Every row must be present, not just the marker: a fixture that threw
        // before reaching a row would otherwise pass on the rows it did reach.
        for row in [
            "shape wrong=",
            "controlLongField wrong=",
            "controlOnePhi wrong=",
            "controlNoBranch wrong=",
            "Duration.toNanos wrong=",
            "Duration.ofMillis(1).toNanos wrong=",
            "truncatedTo threw=",
        ] {
            assert!(
                stdout.contains(row),
                "[jit_ir_phi_copy_register_alias] row {row:?} missing (nojit={nojit}); \
                 the probe did not run to completion\n--- stdout ---\n{stdout}"
            );
        }
        assert!(
            stdout.contains(MARKER),
            "[jit_ir_phi_copy_register_alias] the probe reported a divergence (nojit={nojit})\n\
             --- stdout ---\n{stdout}"
        );
    }
}
