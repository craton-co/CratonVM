// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The RBC.6 lift's differential acceptance test: a once-called method whose
//! hot loop lives in a body containing a bare `athrow` (a `throw` with no local
//! `catch`) must OSR-compile, and must still throw exactly once, after exactly
//! the iterations it really ran.
//!
//! `compile_osr_artifact` used to refuse every `athrow`-containing method
//! outright (`if scan.has_athrow { return None; }`). OSR is the ONLY door out
//! of the interpreter for a method invoked once — what a `@Test` body, a `main`
//! and any one-shot driver is — so a `throw` on a path never taken kept the
//! method's hot loop interpreted for its whole life. That is what made
//! `BOBYQAOptimizerTest` hang: `trsbox`/`bobyqb` are each called once per test
//! and each `throw` a `MathIllegalStateException` on an internal assertion they
//! never catch.
//!
//! ## What the refusal was actually guarding, and why this test is about that
//!
//! Its own comment: "the OSR bail path resumes interpretation at the back-edge,
//! so an athrow lowering that ran side effects natively before throwing could
//! see them re-applied". Re-running committed iterations is RBC.7's
//! silent-corruption shape — a wrong answer no termination test can see. So the
//! assertion that matters here is not "it got faster", it is
//! **`effects == throwAt + 1`, exactly**: the probe increments a static counter
//! on every iteration before the throw can fire, so a stale resume at the
//! back-edge reads HIGH with no exception anywhere.
//!
//! ## Anti-vacuity
//!
//! Two independent guards, because a green run of a loop that never compiled
//! proves nothing:
//!
//!   1. `CRATONVM_DBG_JITC=1` must print an `OSR-compile ... coldThrow` line in
//!      the ON arm — the lift actually admitted the method — and must NOT print
//!      one in the OFF arm.
//!   2. The OFF arm (`CRATONVM_JIT_OSR_ATHROW=0`, the same binary) must produce
//!      byte-identical counts. One binary, one flag: a cross-binary A/B is not
//!      an A/B.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Trimmed from `probes/OsrAthrowProbe.java` — see that file for the full
/// argument. Every arm is called exactly ONCE, which is the whole point: a
/// second call opens the method-entry tier-up door and the measurement stops
/// being about OSR.
const PROBE_SRC: &str = r#"
public final class OsrAthrowLiftProbe {
    static long sink;
    static int effects;
    static int failures;

    static final RuntimeException E = new RuntimeException("probe") {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static int leaf(int i) { return i + 1; }

    static void coldThrower(int i) { if (i < 0) { throw E; } }

    // The BOBYQA shape: bare athrow on a path never taken, no local handler.
    static long coldThrow(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) {
            if (i < 0) { throw E; }
            a += leaf(i);
        }
        return a;
    }

    // The hazard shape: every iteration commits a counted side effect BEFORE
    // the throw can fire, so the count is an exact witness of iterations run.
    static void takenThrow(int n, int throwAt) {
        for (int i = 0; i < n; i++) {
            effects++;
            sink += leaf(i);
            if (i == throwAt) { throw E; }
        }
    }

    // The callee-unwind control: NO `athrow` of its own, so its OSR admission
    // did not change with the lift. It holds the exact-count rule to a shape
    // the lift did not touch.
    static void nestedThrow(int n, int throwAt) {
        for (int i = 0; i < n; i++) {
            effects++;
            sink += leaf(i);
            coldThrower(i == throwAt ? -1 : i);
        }
    }

    static void check(String what, long got, long want) {
        boolean ok = got == want;
        if (!ok) { failures++; }
        System.out.println((ok ? "PASS " : "FAIL ") + what + " got=" + got + " want=" + want);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        int throwAt = (int) (n * 0.6);
        long want = (long) n * (n + 1) / 2;

        check("coldThrow.sum", coldThrow(n), want);

        effects = 0;
        boolean caught = false;
        try { takenThrow(n, throwAt); } catch (RuntimeException e) { caught = e == E; }
        check("takenThrow.caught", caught ? 1 : 0, 1);
        check("takenThrow.effects", effects, throwAt + 1L);
        System.out.println("takenEffects=" + effects);

        effects = 0;
        caught = false;
        try { nestedThrow(n, throwAt); } catch (RuntimeException e) { caught = e == E; }
        check("nestedThrow.caught", caught ? 1 : 0, 1);
        check("nestedThrow.effects", effects, throwAt + 1L);
        System.out.println("nestedEffects=" + effects);

        System.out.println("wantEffects=" + (throwAt + 1));
        System.out.println("sink=" + sink);
        System.out.println("failures=" + failures);
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
    let dir = std::env::temp_dir().join("cratonvm-jit-osr-athrow-lift-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("OsrAthrowLiftProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("OsrAthrowLiftProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_osr_athrow_lift] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_osr_athrow_lift] javac cannot target --release 21 ({}); skipping. \
                 Point JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("OsrAthrowLiftProbe.class").exists(),
        "[jit_osr_athrow_lift] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// One arm. `athrow_lift` selects the gate state; both arms run the SAME
/// binary, which is what makes this an A/B at all.
fn run_probe(bin: &Path, jdk: &Path, classes: &Path, athrow_lift: bool) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // Names both the successful `OSR-compile` line and the `OSR-compile
        // FAILED` refusal, which is how each arm proves what it claims.
        .env("CRATONVM_DBG_JITC", "1")
        .env(
            "CRATONVM_JIT_OSR_ATHROW",
            if athrow_lift { "1" } else { "0" },
        )
        .arg("-c")
        .arg(classes)
        .arg("OsrAthrowLiftProbe")
        .arg("400000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    // Generous: the OFF arm runs the whole probe interpreted, which is the
    // entire point of the bug this test covers.
    let timeout = Duration::from_secs(600);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[jit_osr_athrow_lift] probe timed out after {timeout:?} \
                         (athrow_lift={athrow_lift})"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[jit_osr_athrow_lift] wait failed: {e}"),
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

/// True when this run OSR-compiled `method` (as opposed to refusing it).
///
/// The refusal line is `OSR-compile FAILED <method> ...`, which shares the
/// `OSR-compile ` prefix with the success line — so the negative arm below is
/// only a real check with `FAILED` excluded. Without that, the OFF arm's own
/// refusal satisfied the "did it compile?" predicate and the test reported the
/// two gate states as indistinguishable.
fn osr_compiled(stderr: &str, method: &str) -> bool {
    stderr.lines().any(|l| {
        l.contains("] OSR-compile ") && !l.contains("OSR-compile FAILED") && l.contains(method)
    })
}

#[test]
fn osr_admits_a_bare_athrow_method_and_still_throws_once() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_osr_athrow_lift] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_osr_athrow_lift] no usable JDK found (set CRATONVM_TEST_JDK or JAVA_HOME). \
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

    let (on_out, on_err) = run_probe(&bin, &jdk, &classes, true);
    let (off_out, off_err) = run_probe(&bin, &jdk, &classes, false);

    for (arm, out, err) in [
        ("lift ON", &on_out, &on_err),
        ("lift OFF", &off_out, &off_err),
    ] {
        assert!(
            out.contains("OK"),
            "[jit_osr_athrow_lift] {arm}: probe did not reach its final marker.\n\
             stdout:\n{out}\nstderr:\n{err}"
        );
        assert_eq!(
            field(out, "failures=").as_deref(),
            Some("0"),
            "[jit_osr_athrow_lift] {arm}: the probe's own checks failed.\nstdout:\n{out}"
        );
    }

    // ANTI-VACUITY. Without this, `failures=0` in the ON arm would be satisfied
    // by an interpreter that never compiled anything — the exact trap this
    // whole page of tests exists to avoid.
    assert!(
        osr_compiled(&on_err, "coldThrow"),
        "[jit_osr_athrow_lift] the lift arm never OSR-compiled `coldThrow`, so its green result \
         says nothing about the RBC.6 gate. Did the method get refused for some OTHER reason \
         (the ~30 silent `return None`s in `compile_osr_artifact`)?\nstderr:\n{on_err}"
    );
    assert!(
        !osr_compiled(&off_err, "coldThrow"),
        "[jit_osr_athrow_lift] `CRATONVM_JIT_OSR_ATHROW=0` did NOT restore the refusal — the two \
         arms are the same run and this test can no longer tell the gate states apart.\n\
         stderr:\n{off_err}"
    );

    // THE CORRECTNESS CLAIM. `effects` counts iterations that really ran. A
    // stale resume at the back-edge (the hazard RBC.6 named) re-runs every
    // iteration between OSR entry and the throw, so this reads HIGH — silently,
    // with the exception still delivered and every other assertion green.
    let want = field(&on_out, "wantEffects=").expect("probe must report wantEffects=");
    for (arm, out) in [("lift ON", &on_out), ("lift OFF", &off_out)] {
        for key in ["takenEffects=", "nestedEffects="] {
            assert_eq!(
                field(out, key).as_deref(),
                Some(want.as_str()),
                "[jit_osr_athrow_lift] {arm}: {key} disagrees with the exact iteration count. \
                 A HIGH value is the OSR bail re-running iterations the compiled body had \
                 already committed.\nstdout:\n{out}"
            );
        }
    }

    // And the two arms must agree with each other on every reported number —
    // the lift is a compile-admission change, not a semantics change.
    for key in ["takenEffects=", "nestedEffects=", "sink="] {
        assert_eq!(
            field(&on_out, key),
            field(&off_out, key),
            "[jit_osr_athrow_lift] lift ON and lift OFF disagree on {key}.\n\
             ON:\n{on_out}\nOFF:\n{off_out}"
        );
    }
}
