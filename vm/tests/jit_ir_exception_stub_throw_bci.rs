// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! cov-07 residual: the OPTIMIZING (IR/C2) tier's shared exceptional-exit stub
//! must stamp THIS method's own throw-site bci, or a `finally` in the compiled
//! method is silently skipped.
//!
//! `JitSignals::athrow_bci` is consumed by `execute_jit_call` as the compiled
//! method's throw site and range-tested against `[start_pc, end_pc)` of every
//! entry in that method's own exception table
//! (`find_jit_exception_handler`). When a *dispatched callee* throws, the
//! general `set_jit_pending_exception` resets `athrow_bci` to `-1` — only
//! `jit_throw_exception` (a *local* `athrow`) ever sets a real bci. The
//! compiled caller therefore has to stamp its own invoke bci on the way out.
//!
//! The single-pass backend has done that since RBC.6
//! (`x64/deopt_stubs.rs::emit_exception_check_stub`, one stub per distinct
//! throw-site bci, each calling `jit_set_throw_bci`). The IR tier's
//! `ir_lower::emit_call_exc_stub` emitted ONE shared stub with no stamp, so it
//! returned the `i64::MIN` sentinel with `athrow_bci == -1`.
//!
//! With an unknown pc, `find_jit_exception_handler` honours a catch-all
//! (`catch_type == 0`, i.e. a javac `finally`) ONLY when its protected region
//! spans the whole method — which a `finally` region never does. So the
//! `finally` never runs.
//!
//! cov-07's closeout doc flagged this as a known, pre-existing gap that lane
//! did not own; it became reachable in practice once the exception-table
//! admission relaxation let `try`/`finally` methods onto this tier. See
//! `offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md`.
//!
//! Measured on the fix commit, `n = 200 000`: **198 927** skipped `finally`
//! bodies before, **0** after. HotSpot and `--nojit` are both 0.
//!
//! # This test can skip itself — its codegen twin cannot
//!
//! Everything below needs a built `cratonvm` binary AND a JDK, and returns
//! early when either is missing. On a machine without them this file provides
//! NO coverage, silently. `jit::ir_lower::tests::`
//! `the_exception_stub_stamps_one_set_throw_bci_per_distinct_site` is the
//! unconditional half: pure codegen, no external dependency, and red the
//! instant either the stamp or the per-bci grouping is removed — both verified
//! by injecting each defect and watching it fail, not by trusting a green.
//!
//! Keep both. Only this one shows the stamp actually reaches the interpreter's
//! handler search and runs the `finally`; byte-level assertions cannot.
//!
//! Also note the doc path above moved on retirement, to
//! `fixed-suite-bugs/hibernate/...-FIXED.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// `body` is written to stay C2-admissible, and each constraint is load-bearing:
///
///  * no `putstatic` — opcode `0xb3` has no IR lowering, so the balance counter
///    is an `int[]` cell instead of a static field;
///  * `n[0] = n[0] + 1`, never `n[0]++` — the compound form compiles to `dup2`
///    (`0x5c`), which the IR builder also refuses;
///  * the `finally` handler reads only a PARAMETER local, so RBC.6's
///    `local_handler_reads_unsafe_local` does not force the method back to the
///    single-pass backend (where this defect does not exist);
///  * **the increment is OUTSIDE the `try`** — added 2026-08-21, and the one
///    constraint that is not obvious from reading the Java. See below.
///
/// Any of those slipping would send `body` to a tier that was always correct
/// here, and the test would pass vacuously. The anti-vacuity assertion below is
/// what actually catches that — and on 2026-08-17 it did, for four days, with
/// nobody reading it.
///
/// # Why the increment sits outside the `try`
///
/// `jit::ir_unresumable_protected_trap` (landed 2026-08-17,
/// `unresumable-unconditional-trap-mvmap-FIXED-20260802.md`) refuses the
/// optimizing tier any method whose protected range contains BOTH a
/// deopt-guarded opcode AND a side effect: the IR tier lowers array access to a
/// deopt guard, `can_deopt_resume` is false on a production artifact, and
/// replaying a range that has already committed a store is observably wrong.
/// That rule is correct and this probe used to trip it. With
/// `n[0] = n[0] + 1` inside the `try`, javac emits
///
///     Exception table: from 0 to 12 target 23 any
///     4: iaload        <- deopt-guarded  (the reported trap)
///     7: iastore       <- side effect
///     9: invokestatic  <- side effect
///
/// so the range carries a trap and the method went to the single-pass backend,
/// where this defect does not exist. Hoisting the increment above the `try`
/// leaves the range as
///
///     Exception table: from 8 to 12 target 23 any
///     8: iload_0
///     9: invokestatic
///
/// — an invoke leaves through the `i64::MIN` sentinel and needs no resume, so
/// the rule's first narrowing term excludes it and the tier takes the method
/// again.
///
/// **This does not weaken the probe, and the direction matters.** The defect
/// needs a protected range that does NOT span the whole method, because
/// `find_jit_exception_handler` honours a catch-all only when its region does;
/// the new range is 8..12 of a 35-byte method where the old one was 0..12, so
/// the unknown-pc rule is if anything easier to hit. The balance is unchanged:
/// the increment always runs, the decrement runs only if the `finally` does.
/// Verified the way this tree requires — by BREAKING the thing under test:
/// with the `jit_set_throw_bci` stamp removed from `ir_lower::emit_call_exc_stub`,
/// this probe reports `caught=200000 leaked=397032` and the assertion below
/// fires with its own message.
///
/// `leaked` exceeds `iters` because the increment runs TWICE on a skipped
/// `finally`: once in the compiled frame and again when the interpreter replays
/// the method from entry. That is a detail of the failure, not of the probe —
/// the assertion is `leaked == 0`, and any non-zero value is the defect.
const PROBE_SRC: &str = r#"
public class IrExceptionStubThrowBciProbe {
    // The DISPATCHED callee whose throw exits `body` through the shared stub.
    static void thrower(int i) {
        throw new RuntimeException("x" + i);
    }

    // The protected region covers only the dispatched call, and ends well
    // before the method does — exactly the shape the unknown-pc rule refuses.
    // The increment is deliberately ABOVE the `try` — see the Rust doc comment
    // on PROBE_SRC. Moving it back inside sends this method to the single-pass
    // backend and the test stops proving anything.
    static void body(int i, int[] n) {
        n[0] = n[0] + 1;
        try {
            thrower(i);
        } finally {
            n[0] = n[0] - 1;
        }
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int[] n = new int[1];
        int caught = 0;
        for (int i = 0; i < iters; i++) {
            try {
                body(i, n);
            } catch (RuntimeException e) {
                caught++;
            }
        }
        System.out.println("caught=" + caught);
        System.out.println("leaked=" + n[0]);
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

/// Prerequisite gate: the lookup below is unchanged — only a MISSING JDK is
/// reported differently. See `common::require_jdk`.
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
    let dir = std::env::temp_dir().join("cratonvm-jit-ir-exception-stub-throw-bci-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("IrExceptionStubThrowBciProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("IrExceptionStubThrowBciProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "[jit_ir_exception_stub_throw_bci] javac could not be executed: {e}; skipping"
            );
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_ir_exception_stub_throw_bci] javac cannot target --release 21 ({}); \
                 skipping. Point JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("IrExceptionStubThrowBciProbe.class").exists(),
        "[jit_ir_exception_stub_throw_bci] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // Surfaces "[ir] optimizing backend produced a body for ..." so the
        // test can prove `body` actually took the IR pipeline instead of
        // passing vacuously off the single-pass fallback. Unlike the cov-07
        // sneaky-throw probe this defect is not a tier-transition race — it
        // fires on every compiled invocation — so the extra stderr cannot
        // perturb it away.
        .env("CRATONVM_DBG", "ir-compiles")
        .arg("-c")
        .arg(classes)
        .arg("IrExceptionStubThrowBciProbe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    let timeout = Duration::from_secs(300);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[jit_ir_exception_stub_throw_bci] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[jit_ir_exception_stub_throw_bci] wait failed: {e}"),
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
fn ir_exception_stub_stamps_this_methods_throw_bci() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_ir_exception_stub_throw_bci] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_ir_exception_stub_throw_bci] no usable JDK found (set CRATONVM_TEST_JDK or \
             JAVA_HOME). skipping."
        );
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
        "[jit_ir_exception_stub_throw_bci] probe did not reach its final marker.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Anti-vacuity FIRST: if the optimizing tier never compiled `body`, a green
    // `leaked=0` proves nothing — the interpreter and the single-pass backend
    // have both always been correct here.
    let ir_compiled_body = stderr
        .lines()
        .any(|l| l.contains("optimizing backend produced a body") && l.contains(".body("));
    assert!(
        ir_compiled_body,
        "[jit_ir_exception_stub_throw_bci] `body` was never reported as compiled by the \
         optimizing tier — this run proves nothing about the IR exception stub.\n\
         stderr:\n{stderr}"
    );

    let caught: u64 = field(&stdout, "caught=")
        .and_then(|v| v.parse().ok())
        .expect("probe must report caught=");
    assert!(
        caught > 0,
        "[jit_ir_exception_stub_throw_bci] no iteration threw at all — the probe is not \
         exercising the path it claims to.\nstdout:\n{stdout}"
    );

    assert_eq!(
        field(&stdout, "leaked=").as_deref(),
        Some("0"),
        "an IR-compiled method's `finally` did not run on the exceptional exit: the shared \
         exception stub returned the i64::MIN sentinel without stamping this method's own \
         throw-site bci, so the interpreter routed with throw_pc == usize::MAX and \
         find_jit_exception_handler skipped the catch-all (its protected region does not span \
         the whole method).\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
