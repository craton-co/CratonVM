// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 3 — `Scanner` against `System.in` regression.
//!
//! Pins the S110 fix that wires `System.in` to OS stdin (fd id 0 in the
//! pre-registered `FileDescriptorTable`).
//!
//! Before S110, `native_system_init_phase1` was registered as a no-op stub
//! in real-JDK mode (see the historical comment block in
//! `native-builtins/src/lib.rs`). That left `System.in` as `null`, so
//! `new Scanner(System.in).nextLine()` / `.nextInt()` immediately threw
//! `NoSuchElementException: no more elements` — even when piped data was
//! sitting on stdin.
//!
//! Two real-world Yandex.Disk training apps reproduce this:
//!
//! * `electronic-watch` — three `Scanner.nextInt()` reads + a printf
//! * `meet-a-stranger`  — `Scanner.nextLine()` + a `println("Hello, …")`
//!
//! Acceptance:
//!
//! 1. The triple `(java/lang/System, initPhase1, ()V)` is registered as a
//!    native — its handler installs `System.in`, `System.out`, `System.err`,
//!    and `System.lineSeparator`. The earlier no-op stub left `System.in`
//!    null and broke every Scanner+stdin program in real-JDK mode.
//! 2. Subprocess: `apps/scanner_probe/ScannerProbe.java` reads a line, then
//!    an int, from piped stdin. Output must match the HotSpot reference
//!    (`line=world` + `int=42` + `OK`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::SharedVm;

fn shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

#[test]
fn system_init_phase1_is_registered_as_native() {
    let shared = shared();
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/System", "initPhase1", "()V")
            .is_some(),
        "S110 regression: System.initPhase1()V MUST be registered as a \
         native — the handler installs System.in / out / err. The pre-S110 \
         no-op stub left System.in null, breaking every \
         `new Scanner(System.in)` program in real-JDK mode."
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
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
    let target = workspace_root().join("target");
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

fn java_home() -> Option<PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&jh);
        if p.exists() {
            return Some(p);
        }
    }
    let candidate = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

fn scanner_probe_dir() -> PathBuf {
    workspace_root().join("apps").join("scanner_probe")
}

fn ensure_scanner_probe_compiled() -> bool {
    let dir = scanner_probe_dir();
    let class_file = dir.join("ScannerProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("ScannerProbe.java");
    if !source.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave3_scanner",
            "the Wave 3 scanner fixture `ScannerProbe.java`",
            &[source.clone()],
        );
        return false;
    }
    let compile = Command::new("javac")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            assert!(
                o.status.success(),
                "[wave3_scanner] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// Run the probe with the given stdin payload and return `(stdout, stderr, rc)`.
fn run_scanner_probe(
    stdin_payload: &str,
    timeout: Duration,
) -> Option<(String, String, Option<i32>)> {
    if !ensure_scanner_probe_compiled() {
        eprintln!("wave3_scanner: ScannerProbe.class missing and javac unavailable; skipping");
        return None;
    }
    let bin = cratonvm_binary()?;
    let jh = java_home()?;
    let dir = scanner_probe_dir();

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home")
        .arg(&jh)
        .arg("-c")
        .arg(&dir)
        .arg("ScannerProbe");

    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(stdin_payload.as_bytes());
        // Drop closes the pipe → child sees EOF.
    }

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    };
    let out = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Some((stdout, stderr, status.code()))
}

#[test]
fn scanner_reads_line_and_int_from_stdin() {
    // HotSpot reference: `line=world` + `int=42` + `OK`.
    let Some((stdout, stderr, rc)) = run_scanner_probe("world\n42\n", Duration::from_secs(120))
    else {
        eprintln!(
            "wave3_scanner: prerequisites missing; skipping \
             (set CRATONVM_BIN + JAVA_HOME or build target/release/cratonvm)"
        );
        return;
    };
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        combined.contains("line=world"),
        "ScannerProbe must print `line=world` from `s.nextLine()` — pre-S110 \
         this said `hasNextLine=false` because `System.in` was null; \
         got:\n{combined}"
    );
    assert!(
        combined.contains("int=42"),
        "ScannerProbe must print `int=42` from `s.nextInt()`; got:\n{combined}"
    );
    assert!(
        combined.contains("OK"),
        "ScannerProbe must reach the trailing `OK` print; got:\n{combined}"
    );
    assert_eq!(rc, Some(0), "ScannerProbe must exit 0");
    assert!(
        !combined.contains("NoSuchElementException"),
        "ScannerProbe must not throw NoSuchElementException — that is the \
         exact symptom S110 fixed; got:\n{combined}"
    );
}
