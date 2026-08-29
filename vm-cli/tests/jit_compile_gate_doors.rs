// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `osr-01` step 3: every door into the JIT backend goes through one gate.
//!
//! The `osr-01` lane brief (retired to
//! `osr-01-entry-metadata-contract-RETIRED-20260804.md`) asks for "a single
//! entry point for *produce an OSR-capable artifact*, so the direct path in
//! `invoke.rs` and the `try_compile` path cannot drift again", and
//! `docs/feature-designs/jit-osr-entry-metadata.md` settles the shape: a shared
//! gate function every door must call, with a test that the OSR path calls it.
//! This is that test, and it checks the claim as a **measurement over a real
//! run** rather than as a property of the source text — five checks in this
//! repository named a file where they meant a module and died the day that file
//! was split.
//!
//! What is asserted, and what each one would catch:
//!
//! * `ungated-backend-entries == 0` — nothing reached
//!   `x64::compile_with_param_slots` without an admission token open on the
//!   thread. A fourth door added later, or one of these three refactored back
//!   out of the gate, moves this off zero.
//! * `osr: admitted > 0` — the anti-vacuity half. Without it every assertion
//!   here passes on a run that never compiled anything, which is exactly how a
//!   Spring test class (**zero** OSR entries) made an earlier "0 violations"
//!   reading meaningless.
//! * `eager-first-call: admitted > 0`, in its own arm — that door is dormant
//!   under the default configuration (`bg-compile` is default-ON and reroutes
//!   first-call compiles to the background worker), so the default arm proves
//!   nothing about it. `CRATONVM_JIT=bg-compile=0` is what wakes it.
//!
//!   **And a bytecode workload cannot reach it however hot it gets — which is
//!   what this arm got wrong on 2026-08-06.** The door lives in
//!   `interpreter::execute()`, and `execute()` is not the interpreter's own
//!   invoke path: ordinary `invokestatic` / `invokevirtual` go through the
//!   dispatcher's invoke caches and never call it. `execute()` is the entry
//!   point from OUTSIDE the interpreter loop — reflection, JNI, a native's
//!   `ctx.invoke_*`, VM bootstrap. All three of the probe's original methods
//!   are driven from Java bytecode, so the counter read `admitted=0` in four
//!   configurations and the door was briefly recorded here as dead code. It is
//!   not dead; the probe was asking with the wrong workload.
//!
//!   `reflectedOnly` is what fixed that: a method nothing calls directly,
//!   driven through `Method.invoke`, which is precisely the entered-from-
//!   outside shape. Measured with it, `bg-compile=0` gives
//!   `eager-first-call: admitted=1`. **If this assertion fails again, check
//!   that the probe still reaches `execute()` before concluding the door has
//!   gone** — that was the trap the first time.
//! * `osr-contract-violations == 0` / `osr-coordinate-mismatches == 0` — the
//!   two fail-closed OSR metadata checks. Both are compiler-bug detectors that
//!   silently cost a method its OSR service, so "it never fires" has to be a
//!   measurement.
//!
//! The probe is deliberately the smallest thing that drives all three doors:
//! two hot loops (OSR at their back-edges) and enough distinct callees to make
//! the method-entry and first-call doors fire. Its arithmetic is checked too,
//! because a gate that refused every compile would also satisfy every counter
//! assertion above.

use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class CompileGateDoorsProbe {
    // A hot loop whose back-edge drives an OSR compile of THIS method. The
    // setup local is dead at the loop head, which is the shape the OSR
    // entry metadata contract governs.
    static long hotLoop(int n, String unusedInLoop) {
        int seed = unusedInLoop.length();
        long total = 0;
        for (int i = 0; i < n; i++) {
            total += (i & 0xffff) + seed;
        }
        return total + seed;
    }

    // A second one, with a different shape, so a single refusal cannot make
    // the OSR door look untaken.
    static long hotLoop2(int n) {
        long a = 1;
        long b = 0;
        for (int i = 0; i < n; i++) {
            long t = a + b;
            b = a;
            a = t & 0xffffff;
        }
        return a + b;
    }

    // Called often enough to cross the method-entry tiering threshold
    // without a loop of its own, so it reaches the backend through the
    // ordinary door rather than through OSR.
    static int leaf(int x) {
        return (x * 31) ^ (x >>> 3);
    }

    // REFLECTED ONLY, and that is the whole point: nothing in this class calls
    // it directly, so the only route to it is `Method.invoke` -> the VM's
    // `execute()` entry point, which is where the eager first-call door lives.
    // An ordinary bytecode invoke goes through the dispatcher's invoke caches
    // and never calls `execute()` at all, which is why the three methods above
    // cannot reach that door however hot they get.
    //
    // `public` on purpose. Package-private would be legal for a caller in the
    // same class on HotSpot, and this VM refuses it
    // (`IllegalAccessException: cannot access member: modifiers 0x0008`) --
    // a real defect, but not this probe's subject, and depending on it here
    // would make the gate fail for an unrelated reason.
    public static int reflectedOnly(int x) {
        int acc = x;
        for (int i = 0; i < 32; i++) {
            acc = acc * 31 + (i ^ (acc >>> 3));
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        long l1 = hotLoop(400000, "seedy");
        long l2 = hotLoop2(400000);
        int acc = 0;
        for (int i = 0; i < 200000; i++) {
            acc += leaf(i);
        }
        java.lang.reflect.Method reflected =
            CompileGateDoorsProbe.class.getDeclaredMethod("reflectedOnly", int.class);
        int racc = 0;
        for (int i = 0; i < 2000; i++) {
            racc ^= (Integer) reflected.invoke(null, i);
        }
        System.out.println("hotLoop=" + l1);
        System.out.println("hotLoop2=" + l2);
        System.out.println("leafAcc=" + acc);
        System.out.println("reflectedAcc=" + racc);
        System.out.println("OK");
    }
}
"#;

/// The `cratonvm` binary **this run built**.
///
/// This test lives in `vm-cli` rather than in `vm`, and that is load-bearing.
/// Cargo builds a package's own binary targets before running that package's
/// integration tests, and only that package's. The same test under `vm/tests/`
/// compiles the `vm` library and then runs whatever `cratonvm` happens to be
/// sitting in `target/release` — which may be several commits old. Not
/// hypothetical: the first injection run of this very test (the gate deleted
/// from the OSR door) **passed**, because the stale binary still had the gate
/// in it. Rebuilding the binary and re-running showed what the check really
/// sees — `osr: admitted=0`, `ungated-backend-entries=3`.
/// `CARGO_BIN_EXE_cratonvm` is what cargo sets to the binary it built for this
/// test, and it is the only path here that cannot go stale.
fn cratonvm_binary() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let p = PathBuf::from(env!("CARGO_BIN_EXE_cratonvm"));
    if p.exists() {
        return Some(p);
    }
    None
}

fn jdk_home() -> Option<PathBuf> {
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

/// Compile the probe. Returns `None` only when javac is genuinely unavailable;
/// a probe that fails to COMPILE panics, because "skipped" and "passed" look
/// identical in a suite summary and a broken probe would read as a clean run.
fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-jit-compile-gate-doors-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("CompileGateDoorsProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("CompileGateDoorsProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_compile_gate_doors] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_compile_gate_doors] javac cannot target --release 21 ({}); skipping. \
                 Point JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("CompileGateDoorsProbe.class").exists(),
        "[jit_compile_gate_doors] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(
    bin: &Path,
    jdk: &Path,
    classes: &Path,
    extra_env: &[(&str, &str)],
) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // The admission-gate line rides on the JIT method-stats exit hook.
        .env("CRATONVM_DBG", "jit-method-stats")
        .arg("-c")
        .arg(classes)
        .arg("CompileGateDoorsProbe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn cratonvm");
    // Bounded: a hung run writes no output at all, and an unbounded wait would
    // turn that into a suite that never finishes rather than a failure.
    let timeout = Duration::from_secs(600);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[jit_compile_gate_doors] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[jit_compile_gate_doors] wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect probe output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The admission-gate line, as `key -> value`. Parsed rather than
/// substring-matched so a renamed neighbouring field cannot make an assertion
/// silently stop looking at anything.
fn gate_line(stderr: &str) -> Vec<(String, u64)> {
    let line = stderr
        .lines()
        .find(|l| l.contains("JIT admission gate:"))
        .unwrap_or_else(|| {
            panic!(
                "[jit_compile_gate_doors] no admission-gate line in stderr — the diagnostic \
                 is unconditional inside `dump_method_stats_to_stderr`, so its absence means \
                 the exit hook did not run.\nstderr:\n{stderr}"
            )
        });
    let mut out = Vec::new();
    for tok in line.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if let Ok(n) = v.trim().parse::<u64>() {
                out.push((k.to_string(), n));
            }
        }
    }
    out
}

/// The `n`-th `key=` on the gate line. The per-door fields repeat their names
/// (`admitted=`/`refused=` once per door, in `CompileDoor::ALL` order), so the
/// index IS the door.
fn nth(fields: &[(String, u64)], key: &str, n: usize) -> u64 {
    let hits: Vec<u64> = fields
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| *v)
        .collect();
    assert!(
        hits.len() > n,
        "[jit_compile_gate_doors] expected at least {} `{key}=` fields, found {}: {fields:?}",
        n + 1,
        hits.len()
    );
    hits[n]
}

fn only(fields: &[(String, u64)], key: &str) -> u64 {
    let hits: Vec<u64> = fields
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| *v)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "[jit_compile_gate_doors] expected exactly one `{key}=` field: {fields:?}"
    );
    hits[0]
}

const DOOR_METHOD_ENTRY: usize = 0;
const DOOR_EAGER_FIRST_CALL: usize = 1;
const DOOR_OSR: usize = 2;

fn check_probe_arithmetic(stdout: &str, stderr: &str) {
    // A gate that refused every compile would satisfy every counter assertion
    // in this file. These four lines are what stops that from reading as a
    // pass: they are the interpreter-and-JIT agreement on the same values.
    // Values taken from HotSpot (`java -cp . CompileGateDoorsProbe`, JDK 21),
    // not from this VM — a self-derived expectation would agree with a
    // miscompile.
    for expected in [
        "hotLoop=12909713221",
        "hotLoop2=11295064",
        "leafAcc=1526967200",
        "reflectedAcc=-149467323",
        "OK",
    ] {
        assert!(
            stdout.contains(expected),
            "[jit_compile_gate_doors] probe output missing `{expected}`.\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn every_backend_door_goes_through_the_admission_gate() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_compile_gate_doors] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_compile_gate_doors] no usable JDK found (set CRATONVM_TEST_JDK or \
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

    // ── Arm 1: the default configuration ──────────────────────────────
    let (stdout, stderr) = run_probe(&bin, &jdk, &classes, &[]);
    check_probe_arithmetic(&stdout, &stderr);
    let fields = gate_line(&stderr);

    assert_eq!(
        only(&fields, "ungated-backend-entries"),
        0,
        "[jit_compile_gate_doors] something reached the x64 backend with no admission \
         token open. That is the drift `compile_gate` exists to prevent: a door that \
         skips the gate skips the kill switch, the permanent bail-list, the bisect \
         levers, the code-cache cap AND the compile-epoch witness.\nstderr:\n{stderr}"
    );
    assert!(
        nth(&fields, "admitted", DOOR_OSR) > 0,
        "[jit_compile_gate_doors] the OSR door admitted nothing, so every other \
         assertion here is vacuous — the probe's two hot loops must drive OSR \
         compiles.\nstderr:\n{stderr}"
    );
    assert!(
        nth(&fields, "admitted", DOOR_METHOD_ENTRY) > 0,
        "[jit_compile_gate_doors] the method-entry door admitted nothing.\nstderr:\n{stderr}"
    );
    assert_eq!(
        only(&fields, "osr-contract-violations"),
        0,
        "[jit_compile_gate_doors] an artifact's OSR metadata contradicted itself and was \
         dropped. That is a compiler bug and the method silently lost OSR service.\n\
         stderr:\n{stderr}"
    );
    assert_eq!(
        only(&fields, "osr-coordinate-mismatches"),
        0,
        "[jit_compile_gate_doors] an OSR vector failed its interpreter-bci coordinate \
         conversion — a plausible integer in the wrong pc space.\nstderr:\n{stderr}"
    );

    // Arm 2: the eager first-call door.
    //
    // Dormant under the default configuration: `bg-compile` is default-ON and
    // reroutes a first-call compile to the background worker, so arm 1 says
    // nothing about that door at all. Without this arm the gate could be
    // missing from it entirely and both `admitted=0` and
    // `ungated-backend-entries=0` would still hold.
    //
    // What makes the assertion below reachable is the probe's `reflectedOnly`,
    // not this environment alone — see the module doc. A bytecode-driven
    // workload cannot take this door at any temperature.
    let (stdout, stderr) = run_probe(&bin, &jdk, &classes, &[("CRATONVM_JIT", "bg-compile=0")]);
    check_probe_arithmetic(&stdout, &stderr);
    let fields = gate_line(&stderr);
    assert_eq!(
        only(&fields, "ungated-backend-entries"),
        0,
        "[jit_compile_gate_doors] ungated backend entry under bg-compile=0.\nstderr:\n{stderr}"
    );
    assert!(
        nth(&fields, "admitted", DOOR_EAGER_FIRST_CALL) > 0,
        "[jit_compile_gate_doors] the eager first-call door admitted nothing even with          `bg-compile=0`. Before concluding the path is gone, check that the probe still          reaches `interpreter::execute()` at all: that is the only route to this door,          and it is reflection / JNI / native-invoke only, never a bytecode invoke.          `reflectedOnly` is what supplies it.
stderr:
{stderr}"
    );
}
