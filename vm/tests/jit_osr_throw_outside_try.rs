// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An exception raised OUTSIDE every protected range of an OSR'd method must
//! not be taken by a handler that does not cover it.
//!
//! Since the RBC.6b lift a method with an exception table can be OSR-compiled.
//! When such a body throws, `route_osr_exception_out_of_artifact` asks this
//! method's own table with the PRECISE throw bci and can answer `Propagate` —
//! "this frame cannot catch". The dispatch loop then handed the throwable to
//! `unwind_to_handler` keyed on `entry_pc`, the BACK-EDGE the body was ENTERED
//! at, which has nothing to do with where the throw happened. With the loop
//! inside the `try` (`try { for (..) {..} } catch`) that back-edge IS inside a
//! protected range, so the unwinder found the `catch`, entered it, and resumed
//! the frame on the stale pre-OSR locals.
//!
//! Measured before the fix, against HotSpot on the same probe:
//!
//! ```text
//! HotSpot   caught=0 escaped=1 sink=80000200000
//! CratonVM  caught=1 escaped=1 sink=80018203000
//! ```
//!
//! Both halves are silent: an exception swallowed by a handler that does not
//! guard it, and an accumulator 18 003 000 too high from the iterations the
//! spurious resume re-ran. Neither raises anything, and no termination test can
//! see either.
//!
//! `OSR_FRAME_DECLINED_TO_CATCH` is the fix: a pc no `[start_pc, end_pc)` can
//! contain, so the unwinder's first search matches nothing and it starts at the
//! caller — which is what "the frame already declined" means.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// `probes/OsrThrowOutsideTryProbe.java`. The accumulator is a LOCAL and the
/// static write happens after the loop on purpose: a `putstatic` inside the
/// protected range is refused OSR admission outright
/// (`osr-DENY (osr-exc-site-unpublished … opcode=0xb3)`), which would make this
/// measure the interpreter and pass vacuously.
const PROBE_SRC: &str = r#"
public final class OsrThrowOutsideTryTest {
    static long sink;
    static int caught;
    static int escaped;

    static final RuntimeException E = new RuntimeException("outside") {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static int leaf(int i) { return i + 1; }

    static void trip() { throw E; }

    static void afterLoop(int n) {
        long a = 0;
        try {
            for (int i = 0; i < n; i++) {
                a += leaf(i);
            }
        } catch (RuntimeException e) {
            caught++;
        }
        sink += a;
        trip();
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        try {
            afterLoop(n);
        } catch (RuntimeException e) {
            if (e == E) { escaped++; }
        }
        System.out.println("caught=" + caught);
        System.out.println("escaped=" + escaped);
        System.out.println("sink=" + sink);
        System.out.println("wantSink=" + ((long) n * (n + 1) / 2));
        System.out.println("OK");
    }
}
"#;

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

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-jit-osr-throw-outside-try-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("OsrThrowOutsideTryTest.java");
    let _ = std::fs::remove_file(dir.join("OsrThrowOutsideTryTest.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_osr_throw_outside_try] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_osr_throw_outside_try] javac cannot target --release 21 ({}); skipping.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("OsrThrowOutsideTryTest.class").exists(),
        "[jit_osr_throw_outside_try] the embedded probe failed to compile. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // Names the `OSR-compile` line the anti-vacuity guard below requires.
        .env("CRATONVM_DBG_JITC", "1")
        .arg("-c")
        .arg(classes)
        .arg("OsrThrowOutsideTryTest")
        .arg("400000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    let timeout = Duration::from_secs(600);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[jit_osr_throw_outside_try] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[jit_osr_throw_outside_try] wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect probe output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn field(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}

#[test]
fn an_osr_frame_that_declined_to_catch_does_not_catch_on_the_way_out() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[jit_osr_throw_outside_try] cratonvm binary not found; skipping.");
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[jit_osr_throw_outside_try] no usable JDK found; skipping.");
        return;
    };
    let javac = jdk.join(if cfg!(windows) {
        "bin/javac.exe"
    } else {
        "bin/javac"
    });
    let Some(classes) = compile_probe(&javac) else {
        return;
    };

    let (stdout, stderr) = run_probe(&bin, &jdk, &classes);
    assert!(
        stdout.contains("OK"),
        "[jit_osr_throw_outside_try] probe did not finish.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ANTI-VACUITY FIRST. `afterLoop` interpreted gives the right answer for
    // free — the bug only exists for an OSR'd frame — so a green run that never
    // OSR-compiled the method proves nothing at all.
    let osr_compiled = stderr.lines().any(|l| {
        l.contains("] OSR-compile ") && !l.contains("OSR-compile FAILED") && l.contains("afterLoop")
    });
    assert!(
        osr_compiled,
        "[jit_osr_throw_outside_try] `afterLoop` was never OSR-compiled, so this run says nothing \
         about the OSR unwind path. A refusal line (`osr-DENY`) in the stderr below names why — \
         a `putstatic` or other unpublishable site drifting into the protected range is the usual \
         cause, and the probe puts its accumulator in a LOCAL to avoid exactly that.\n\
         stderr:\n{stderr}"
    );

    assert_eq!(
        field(&stdout, "caught=").as_deref(),
        Some("0"),
        "an exception thrown AFTER the try block was taken by that try's handler. The OSR bail \
         handed it to `unwind_to_handler` keyed on the back-edge `entry_pc` instead of on the \
         throw site.\nstdout:\n{stdout}"
    );
    assert_eq!(
        field(&stdout, "escaped=").as_deref(),
        Some("1"),
        "the throwable did not reach `main` at all.\nstdout:\n{stdout}"
    );
    // The second, quieter half: entering that handler resumed the frame on the
    // stale pre-OSR locals, so the accumulator carried the iterations the
    // resume re-ran. This reads HIGH, never low.
    assert_eq!(
        field(&stdout, "sink=").as_deref(),
        field(&stdout, "wantSink=").as_deref(),
        "the accumulator disagrees with the closed form — a spurious handler entry resumed the \
         frame on stale locals and re-ran committed iterations.\nstdout:\n{stdout}"
    );
}
