// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! An IR **site trap** must never be planted where taking it would re-run a
//! side effect.
//!
//! # The hazard this locks down
//!
//! A trap taken in a compiled body stashes a reconstructed frame and returns
//! the `i64::MIN` sentinel. Three sinks consume that stash, and they do not
//! agree. `try_resume_trapped_callee` and `execute`'s tier-up sink resume the
//! frame precisely. `jit_bridge`'s `jit-callsite-a` / `jit-callsite-b` — the
//! sinks a method reaches when it is called from ordinary bytecode *after* it
//! already has an artifact — return `Ok(None)` / `CacheMiss`, which means
//! **re-execute this method from bci 0**. For a body that already committed a
//! store, a call or a monitor action before the trap, that duplicates it, and
//! nothing says so: it is a silent wrong answer, not an abort.
//!
//! Measured on the probe below, before either fix: `sink=200241` for `200000`
//! calls, with `CRATONVM_DBG_DEOPT=1` naming `jit-callsite-a` 241 times.
//!
//! Both ends are now fixed, independently and on the same day:
//! `CRATONVM_JIT_DEOPT_SINK_RESUME` makes those sinks resume the frame instead
//! of re-running the body, and the guard this file tests stops the trap being
//! planted at all. So the guard-off arm no longer produces a wrong ANSWER, and
//! this test asserts the MECHANISM rather than the symptom: with the guard on
//! the trap is refused, with it off the same site is still offered. Asserting
//! the wrong answer would make this test depend on the sink fix staying
//! broken, which is the opposite of what it is for.
//!
//! # Why the fix is in the compiler and this test is at the Java level
//!
//! `IrBuilder::plant_uncommon_trap` now refuses to plant a trap the
//! interpreter could not get back from — asking
//! `replay_from_entry_is_observably_equivalent`'s own rule at the producing
//! end — so the sinks never see such a frame at all. That is the same
//! compiler-side strategy `ir_unresumable_protected_trap` uses for its
//! narrower shape, and it leaves the two hot `jit_bridge` sinks untouched.
//!
//! It has to be tested from Java. `vm.invoke` enters through `execute`, which
//! is the sink that resumes precisely — a Rust-driven fixture passes whether
//! or not the bug is present. Only ordinary bytecode dispatch into an
//! already-compiled callee reaches `jit-callsite-a`.
//!
//! # The four walls the probe has to clear, and they are all compiler
//! properties rather than workload ones
//!
//!   1. **C1 is a dead end.** The tiered manager's C2 door wants 20 000
//!      invocations and the interpreter's counter stops advancing once the
//!      method is compiled at C1, so more iterations do nothing. Put C1 out of
//!      reach and take the Interpreter -> C2 door.
//!   2. **`new` refuses the body** without `CRATONVM_JIT_C2_ALLOC_UPGRADE`, so
//!      no `new StringBuilder()`.
//!   3. **`putstatic` refuses the body** — `no lowering for opcode 0xb3` — so
//!      the obvious side effect (bump a static) is the one that cannot be
//!      used. An array store is `opcode_commits_side_effect` just the same.
//!   4. **A method whose only call is the `invokedynamic` is never
//!      invoke-planned**, and `indy_trap_sites` is populated inside the
//!      invoke-planning block, so the 0xba arm bails on the
//!      `indy_trap_sites.get(&pc)` miss and no trap is ever planted. `hot`
//!      therefore carries one ordinary `invokestatic` before the concat.
//!
//! Walls 1-3 are `probes/IndySiteTrapSinkProbe.java`'s findings; wall 4 is
//! why that file could not finish the job and this one can.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

/// `hot` commits a side effect (an `invokestatic` and an array store) and then
/// hits an `invokedynamic` the optimizing tier cannot lower. `SINK[0]` must
/// equal `ITERS` exactly; a whole-method replay from entry counts an iteration
/// twice.
const PROBE_SRC: &str = r#"
public class IndySiteTrapSinkProbe2 {
    static final int[] SINK = new int[1];

    // An ordinary `invokestatic`, so the method is invoke-planned at all
    // (wall 4) -- and a committed side effect in its own right.
    static int bump(int i) {
        return (i & 1) + 1 - (i & 1);
    }

    // Side effect BEFORE the `invokedynamic` concat. No `new` (wall 2), no
    // `putstatic` (wall 3).
    static String hot(int i, String s) {
        SINK[0] = SINK[0] + bump(i);
        return s + "-" + i;
    }

    public static void main(String[] args) {
        final int ITERS = Integer.getInteger("probe.iters", 200000);
        final String base = "x";
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            acc += hot(i, base).length();
        }
        System.out.println("sink=" + SINK[0]);
        System.out.println("expected=" + ITERS);
        System.out.println("acc=" + acc);
        System.out.println("OK");
    }
}
"#;

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


fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-site-trap-sink-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("IndySiteTrapSinkProbe2.java");
    // Never let a stale .class stand in for a source that no longer compiles.
    let _ = std::fs::remove_file(dir.join("IndySiteTrapSinkProbe2.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[site_trap_sink] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[site_trap_sink] javac cannot target --release 21 ({}); skipping.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("IndySiteTrapSinkProbe2.class").exists(),
        "[site_trap_sink] the embedded probe failed to compile.\njavac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}


fn field(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}


fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(25);
    lines[start..].join("\n")
}
