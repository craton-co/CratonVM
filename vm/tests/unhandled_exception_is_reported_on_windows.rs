// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression (gcj j3/rf3): on Windows, an exception that ends the process
//! but is NOT on the crash handler's fatal list must still be reported.
//!
//! # What broke
//!
//! `crash_handler::windows_fault`'s vectored handler reports only the codes
//! `is_fatal` lists (access violation, in-page error, illegal / privileged
//! instruction, divide by zero, stack overflow, fast-fail); it has to ignore
//! the rest, because a vectored handler sees every FIRST-chance exception,
//! Rust unwinds included. Nothing else was installed, so any other exception
//! nobody handled ended the VM with its code as the exit status and an empty
//! stderr. `STATUS_GUARD_PAGE_VIOLATION` (0x80000001, a read of another
//! thread's stack guard page) is the dangerous one: Git Bash keeps the low
//! byte of a code below 0xC0000000, so it reads as a bare `rc=1`, which is
//! the `pf_gr_cg_r3` record that started this.
//!
//! # What this asserts
//!
//! The probe reads a fresh `PAGE_GUARD` page through FFM -- a real
//! `STATUS_GUARD_PAGE_VIOLATION` raised in the VM's own code -- and the test
//! requires, by default, the one-line `unhandled exception 0x80000001` notice,
//! the fatal-error banner naming `EXCEPTION_GUARD_PAGE` (the name HotSpot's
//! own report uses), one `hs_err_pid<pid>.log`, and the exit status still
//! being the exception code. With the `=0` kill the old silent death comes
//! back, which proves the filter is what prints.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

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
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join("cratonvm.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// The `guard` case of `tools/probes/GcjUnhandledExceptionProbe.java`.
const PROBE: &str = r#"
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;

public class GuardPageTouch {
    public static void main(String[] args) throws Throwable {
        SymbolLookup k = SymbolLookup.libraryLookup("kernel32", Arena.global());
        MethodHandle alloc = Linker.nativeLinker().downcallHandle(
                k.find("VirtualAlloc").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS,
                        ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        // MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE | PAGE_GUARD
        MemorySegment p = (MemorySegment) alloc.invokeExact(MemorySegment.NULL, 4096L,
                0x3000, 0x104);
        System.out.println("ALLOCATED " + (p.address() != 0));
        System.out.flush();
        long v = p.reinterpret(4096).get(ValueLayout.JAVA_LONG, 0);
        System.out.println("SURVIVED " + v);
    }
}
"#;

const STATUS_GUARD_PAGE_VIOLATION: u32 = 0x8000_0001;

struct Run {
    code: u32,
    stdout: String,
    stderr: String,
    reports: usize,
}

fn run(bin: &Path, classes: &Path, kill: bool) -> Run {
    let cwd = classes.join(if kill { "run-kill" } else { "run-default" });
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).expect("run dir");
    let mut cmd = Command::new(bin);
    cmd.arg("--enable-native-access=ALL-UNNAMED")
        .arg("-cp")
        .arg(classes)
        .arg("GuardPageTouch")
        .env_remove("CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT")
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if kill {
        cmd.env("CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT", "0");
    }
    let child = cmd.spawn().expect("spawn cratonvm");
    let out = common::wait_draining(child, Duration::from_secs(180));
    assert!(!out.timed_out, "the probe did not end within 180 s");
    let reports = std::fs::read_dir(&cwd)
        .expect("read run dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("hs_err_pid"))
        .count();
    Run {
        code: out.output.status.code().unwrap_or(0) as u32,
        stdout: String::from_utf8_lossy(&out.output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.output.stderr).into_owned(),
        reports,
    }
}

#[test]
fn a_guard_page_violation_is_reported_and_keeps_its_exit_code() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[unhandled_exception_is_reported_on_windows] no cratonvm binary; skipping");
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[unhandled_exception_is_reported_on_windows] no usable JDK; skipping");
        return;
    };

    let dir = std::env::temp_dir().join(format!("cratonvm-unhandled-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let src = dir.join("GuardPageTouch.java");
    std::fs::write(&src, PROBE).expect("write probe");
    let out = Command::new(jdk.join("bin").join("javac.exe"))
        .arg("-d")
        .arg(&dir)
        .arg(&src)
        .output()
        .expect("run javac");
    assert!(
        out.status.success(),
        "the GuardPageTouch probe failed to compile:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let on = run(&bin, &dir, false);
    assert!(
        on.stdout.contains("ALLOCATED true"),
        "the probe never reached the guard page; stdout {:?}, stderr:\n{}",
        on.stdout,
        on.stderr
    );
    assert_eq!(
        on.code, STATUS_GUARD_PAGE_VIOLATION,
        "the exit status must stay the exception code; stderr:\n{}",
        on.stderr
    );
    assert!(
        on.stderr
            .contains("[cratonvm] fatal: unhandled exception 0x80000001"),
        "no unhandled-exception notice. stderr:\n{}",
        on.stderr
    );
    assert!(
        on.stderr.contains("A fatal error has been detected")
            && on.stderr.contains("EXCEPTION_GUARD_PAGE (0x80000001)"),
        "no fatal-error banner naming EXCEPTION_GUARD_PAGE. stderr:\n{}",
        on.stderr
    );
    assert_eq!(on.reports, 1, "expected exactly one hs_err_pid<pid>.log");

    let off = run(&bin, &dir, true);
    assert_eq!(off.code, STATUS_GUARD_PAGE_VIOLATION);
    assert!(
        !off.stderr.contains("unhandled exception")
            && !off.stderr.contains("A fatal error has been detected"),
        "with CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT=0 nothing may be reported. stderr:\n{}",
        off.stderr
    );

    let _ = std::fs::remove_dir_all(&dir);
}
