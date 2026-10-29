// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: a hardware fault must produce a crash report, on the binary
//! CI actually drives.
//!
//! # What broke, and why no existing test saw it
//!
//! `crash_signal_handler` is registered with `SA_ONSTACK`, so it runs on the
//! sigaltstack Rust installs per thread -- the `libc` crate's compile-time
//! `SIGSTKSZ`, 8 192 bytes on linux-gnu. The kernel spends part of that on
//! the signal frame before the handler's first instruction
//! (`sysconf(_SC_MINSIGSTKSZ)` measured 3 376 on the x86-64 build host, and
//! `_SC_SIGSTKSZ` recommends 13 504, i.e. glibc itself says 8 192 is no
//! longer enough), leaving roughly 4 816.
//!
//! rustc reserves a function's WHOLE frame in its prologue. While the report
//! path lived inside the registered handler, that prologue was
//! `sub $0x1000` (the stack probe) + `sub $0xbb8` = **7 096 bytes** on a debug
//! build and `sub $0x788` + six pushes = **1 976** on a release one. Release
//! fitted. Debug did not: its first probe store landed below the altstack,
//! faulting while already handling SIGSEGV, at which point the kernel resets
//! to `SIG_DFL` and kills the process -- no banner, no `hs_err_pid<pid>.log`,
//! and nothing on stderr to say why.
//!
//! Two months of debug builds had no crash diagnostics at all, and the second
//! consequence was worse: `cratonvm_jit::implicit_null::recover` is called
//! from that handler, so every receiver null check the JIT had ELIDED (it
//! elides them because `note_fault_handler_installed` was told the handler
//! existed) turned a Java `NullPointerException` into a silent process death.
//! `regression-suite/src/RJitUnrollImplicitNpe.java` had been failing on it,
//! and `ci.yml` drives the regression suite with `CV=target/debug/cratonvm`.
//!
//! Nothing caught it because every test that would have is either a unit test
//! (same process, no fault) or reads an exit code rather than the report, and
//! a killed process and a reported-then-killed process have the same exit
//! code: 139.
//!
//! # What this asserts
//!
//! Start the VM, let it reach steady state, send it `SIGSEGV`, and require
//! the banner on stderr. `kill` is deliberate rather than provoking a real
//! fault: it sets `si_code = SI_USER`, which `si_code_means_a_faulting_address`
//! rejects, so the implicit-null arm is skipped and this measures the REPORT
//! path and nothing else. No JIT is involved -- the workload runs under
//! `CRATONVM_DISABLE_JIT=1`.
//!
//! Unix only. Windows delivers faults to a vectored exception handler that
//! runs on the faulting thread's own stack, so the failure mode does not
//! exist there.

#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        let candidate = target.join(profile).join("cratonvm");
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

/// A workload that runs long enough to be signalled in a steady state and
/// prints as it goes, so the test can wait for "started" rather than sleep a
/// guessed interval.
const SPIN: &str = r#"
public class CrashSpin {
    public static void main(String[] args) throws Exception {
        System.out.println("READY");
        System.out.flush();
        long s = 0;
        for (long i = 0; i < 40_000_000_000L; i++) {
            s += i;
            if ((i & 0xFFFFFFL) == 0) { Thread.sleep(1); }
        }
        System.out.println("done " + s);
    }
}
"#;

#[test]
fn a_hardware_fault_still_prints_a_crash_report() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[crash_handler_reports_a_fault] no cratonvm binary; skipping");
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[crash_handler_reports_a_fault] no usable JDK; skipping");
        return;
    };

    let dir = std::env::temp_dir().join(format!("cratonvm-crashreport-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let src = dir.join("CrashSpin.java");
    std::fs::write(&src, SPIN).expect("write probe");

    let javac = jdk.join("bin").join("javac");
    let out = Command::new(&javac)
        .arg("-d")
        .arg(&dir)
        .arg(&src)
        .output()
        .expect("run javac");
    // The phrasing matters: `vm/tests/probe_compile_guard.rs` requires every
    // test that launches javac to carry an assertion saying "failed to
    // compile", so a probe that stops compiling panics with javac's stderr
    // instead of quietly reporting `ok` forever.
    assert!(
        out.status.success(),
        "the CrashSpin probe failed to compile:
{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `CRATONVM_DISABLE_JIT=1` keeps the JIT, and therefore the implicit-null
    // arm, entirely out of this: what is measured is the report path.
    let mut child = Command::new(&bin)
        .arg("-cp")
        .arg(&dir)
        .arg("CrashSpin")
        .env("CRATONVM_DISABLE_JIT", "1")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cratonvm");

    // Drain BOTH pipes on their own threads. A piped stderr that nobody
    // reads fills and blocks the child, and the VM writes tracing lines long
    // before it writes the banner -- so collecting after `wait()` would
    // deadlock exactly the process this test needs to keep running.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (otx, orx) = std::sync::mpsc::channel();
    let (etx, erx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = otx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        let _ = etx.send(buf);
    });

    // Let it reach steady state. A signal delivered during start-up would be
    // testing something else, and on a loaded shared host start-up is where
    // the variance is. The liveness check turns "it exited early" into a
    // clear failure instead of a kill against a dead pid.
    let settle = Instant::now() + Duration::from_secs(8);
    while Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(st) = child.try_wait().expect("try_wait") {
            panic!("the VM exited ({st:?}) before it could be signalled");
        }
    }

    // SAFETY: `kill(2)` on a child this process spawned and has not reaped.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGSEGV) };
    assert_eq!(rc, 0, "kill(SIGSEGV) failed");

    let status = child.wait().expect("wait");
    let stdout_text = orx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_default();
    let stderr = erx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_default();

    assert!(
        stdout_text.contains("READY"),
        "the probe never started; stdout was {stdout_text:?}, stderr {stderr:?}"
    );

    // The property. Before the 2026-09-21 fix a debug build printed NOTHING
    // here -- the handler died in its own prologue on the 8 KiB alternate
    // stack -- while a release build printed the whole banner, which is why
    // this has to run against the debug binary to mean anything.
    assert!(
        stderr.contains("A fatal error has been detected"),
        "no crash banner after SIGSEGV (exit {status:?}). The handler did not run, \
         or died before its first write. stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("this signal was SENT, not raised by a fault"),
        "the banner did not classify a sent signal, so si_code was misread. stderr:\n{stderr}"
    );

    let reports: Vec<_> = std::fs::read_dir(&dir)
        .expect("read scratch dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("hs_err_pid"))
        .collect();
    assert_eq!(
        reports.len(),
        1,
        "expected exactly one hs_err_pid<pid>.log in {}, found {}",
        dir.display(),
        reports.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
