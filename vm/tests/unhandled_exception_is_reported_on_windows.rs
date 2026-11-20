// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for cand in [
        "/home/victor/jdk25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}


/// The `guard` case of `GcjUnhandledExceptionProbe.java`.
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


