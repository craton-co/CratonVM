// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! cov-07 companion: a method the OPTIMIZING (IR/C2) tier compiled with an
//! `athrow` in it must be entered through the dispatch-aware slow path.
//!
//! `jit_throw_exception` stashes the throwable and returns the `i64::MIN`
//! sentinel, but — unlike every other sentinel producer — it does NOT set
//! `JIT_DEOPT_PENDING`. The dispatch-aware entry does not need it to: that path
//! drains `sig.exception` and routes it unconditionally. The `!has_dispatch`
//! fast entry has no such drain — it consults `deopt_signaled`, which is
//! therefore false — so the stashed exception is silently dropped and the
//! method returns as though it had completed normally.
//!
//! The single-pass backend has held the matching invariant since RBC.6
//! (`emitted_athrow` forces `has_dispatch`). cov-07 admitted `athrow` to the IR
//! tier without bringing that arm across, so an IR-compiled `athrow` method
//! could take the fast entry and swallow its own throw.
//!
//! ## Why the probe forces the caller interpreted
//!
//! The gap governs exactly one edge: **compiled callee → interpreted caller**.
//! Once the caller is also compiled, its own JIT-to-JIT exception routing
//! handles the sentinel and the bug is masked. In a plain hot loop that leaves
//! only the brief window where the callee is compiled and the caller is not
//! yet, so the defect surfaces roughly ONCE per run, near first compilation —
//! reproducible, but far too rare to assert on without flaking.
//!
//! `CRATONVM_JIT_DENY` pins the caller to the interpreter, which holds that
//! edge open for the whole run and turns a once-per-run race into a
//! near-every-iteration certainty. Measured on the fix commit: **1 792 397**
//! swallowed throws in 1 800 000 iterations before, **0** after.
//!
//! The probe is JUnit Platform's sneaky-throw idiom reduced to two methods,
//! because that is what found it: `throwAs` is `checkcast <erased>; athrow` and
//! nothing else, so when the throw is swallowed `throwAsUnchecked` falls
//! through to its `return null`, the caller throws that null, and the ORIGINAL
//! exception is replaced by a helpful-NPE. See
//! `offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class IrAthrowDispatchProbe {
    // `checkcast <erased-to-Throwable>; athrow` and nothing else — a void
    // method whose entire body is an unconditional throw. This is the method
    // the optimizing tier compiles and the one that must not be entered
    // through the no-drain fast path.
    @SuppressWarnings("unchecked")
    private static <T extends Throwable> void throwAs(Throwable t) throws T {
        throw (T) t;
    }

    static RuntimeException throwAsUnchecked(Throwable t) {
        IrAthrowDispatchProbe.<RuntimeException>throwAs(t);
        return null; // unreachable IF throwAs really throws
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int swallowed = 0;
        int thrown = 0;
        for (int i = 0; i < n; i++) {
            try {
                throw throwAsUnchecked(new RuntimeException("probe" + i));
            } catch (NullPointerException npe) {
                // throwAs returned normally => throwAsUnchecked returned null
                // => we just threw null. The real exception is gone.
                swallowed++;
            } catch (RuntimeException e) {
                thrown++;
            }
        }
        System.out.println("swallowed=" + swallowed);
        System.out.println("thrown=" + thrown);
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
    let dir = std::env::temp_dir().join("cratonvm-jit-ir-athrow-dispatch-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("IrAthrowDispatchProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("IrAthrowDispatchProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_ir_athrow_dispatch] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_ir_athrow_dispatch] javac cannot target --release 21 ({}); skipping. \
                 Point JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("IrAthrowDispatchProbe.class").exists(),
        "[jit_ir_athrow_dispatch] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // Hold open the one edge this gap governs: compiled callee, INTERPRETED
        // caller. Without this the defect fires about once per run (only while
        // the callee is compiled and the caller is not yet), which is real but
        // far too rare to assert on.
        .env(
            "CRATONVM_JIT_DENY",
            "IrAthrowDispatchProbe.throwAsUnchecked",
        )
        // Surfaces "[ir] optimizing backend produced a body for ..." so the
        // test can prove `throwAs` actually took the IR pipeline instead of
        // passing vacuously off the single-pass fallback.
        .env("CRATONVM_DBG", "ir-compiles")
        // WITHOUT THIS THE TEST CANNOT PASS, and it could not from 2026-09-06
        // (when `CRATONVM_C2_ACCEPT` defaulted to `evidence`) until 2026-09-08.
        //
        // `throwAs` is `checkcast; athrow` and nothing else. The C1→C2
        // acceptance gate publishes an optimizing body only when it carries a
        // transform the baseline lacks — `is_worth_publishing`: scalar
        // replacement, inlining, an elided guard, a sunk allocation, a scalar
        // intrinsic, or a simplified graph. A two-instruction unconditional
        // throw earns NONE of those **by construction**, so the gate refused it
        // (`[ir] acceptance …: REFUSED (evidence: none) -- keeping the
        // single-pass body`) on every run, and the anti-vacuity assertion below
        // fired every time. That assertion doing its job is the only reason
        // anyone could tell: without it this file would have gone on reporting
        // `ok` while measuring the single-pass backend, which has always been
        // correct here.
        //
        // Pinning the policy is the right fix rather than widening the gate:
        // this file is about the IR tier's `athrow` LOWERING, not about the
        // gate's throughput judgment, and `ir_evidence::accept_policy`'s own
        // doc nominates `=always` as the arm to take a claim against.
        .env("CRATONVM_C2_ACCEPT", "always")
        .arg("-c")
        .arg(classes)
        .arg("IrAthrowDispatchProbe")
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
                    panic!("[jit_ir_athrow_dispatch] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[jit_ir_athrow_dispatch] wait failed: {e}"),
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
fn ir_athrow_method_is_entered_through_dispatch() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_ir_athrow_dispatch] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_ir_athrow_dispatch] no usable JDK found (set CRATONVM_TEST_JDK or JAVA_HOME). \
             skipping."
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
        "[jit_ir_athrow_dispatch] probe did not reach its final marker.\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );

    // Anti-vacuity FIRST: if the optimizing tier never compiled `throwAs`, a
    // green `swallowed=0` proves nothing — it would just be the interpreter
    // (or the single-pass backend, which has always been correct here) doing
    // the work. This is the trap the cov-06 probe documents.
    let ir_compiled_thrower = stderr
        .lines()
        .any(|l| l.contains("optimizing backend produced a body") && l.contains("throwAs"));
    assert!(
        ir_compiled_thrower,
        "[jit_ir_athrow_dispatch] `throwAs` was never reported as compiled by the optimizing \
         tier — this run proves nothing about the IR athrow lowering.\nstderr:\n{stderr}"
    );

    let thrown: u64 = field(&stdout, "thrown=")
        .and_then(|v| v.parse().ok())
        .expect("probe must report thrown=");

    assert_eq!(
        field(&stdout, "swallowed=").as_deref(),
        Some("0"),
        "an IR-compiled `athrow` returned normally instead of throwing: the compiled body \
         stashed the throwable and returned the i64::MIN sentinel, but the method was entered \
         through the `!has_dispatch` fast path, which never drains it. The real exception is \
         lost and the caller sees a helpful-NPE from throwing the resulting null.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        thrown > 0,
        "[jit_ir_athrow_dispatch] no iteration threw at all — the probe is not exercising the \
         path it claims to.\nstdout:\n{stdout}"
    );
}
