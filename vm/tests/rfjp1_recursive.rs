// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RFJP.1 — `apps/fjp_probe/FjpProbe` (`pool.invoke(RecursiveTask)` of a
//! divide-and-conquer Long sum) must print `sum = 499999500000` and `OK`.
//!
//! Pre-fix the JIT compiled the recursive `compute()` returning Long and
//! a regalloc clobber at depth >= 10 caused the probe to print `sum = 0`.
//! The workaround landed in `vm/src/runtime/interpreter.rs::class_extends_forkjointask`,
//! which forces interpreter-only execution for any method whose declaring
//! class is in the ForkJoinTask family
//! (ForkJoinTask / RecursiveTask / RecursiveAction / CountedCompleter).
//!
//! Why this is a real-JDK-mode pin (not a `vm.invoke` Rust unit test):
//!   * The bug only reproduces when JIT-compiled bytecode of
//!     `RecursiveTask.compute()` runs against the real Adoptium 25
//!     ForkJoinTask hierarchy. Synthetic-JDK mode has its own stub
//!     implementations and does not exercise the JIT regalloc path.
//!   * Therefore the test spawns the freshly-built `cratonvm.exe` as a
//!     subprocess. Skips when neither the binary nor a JDK 25
//!     `java-home` is available.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the freshly-built CLI binary the harness should exercise.
/// Honors `CRATONVM_BIN` for callers that want to point at a custom
/// build; otherwise resolves to the workspace's
/// `target/release/cratonvm.exe`.
mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CRATONVM_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let candidate = PathBuf::from(manifest_dir)
        .parent()
        .map(|p| p.join("target").join("release").join("cratonvm.exe"))?;
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Resolve the real-JDK 25 java-home — env var first, then the standard
/// Adoptium install path. Returns None when none is reachable so the
/// test can skip cleanly on machines without the JDK 25 dependency.
fn real_java_home() -> Option<String> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        if Path::new(&jh).join("bin/java.exe").exists() || Path::new(&jh).join("bin/java").exists()
        {
            return Some(jh);
        }
    }
    let adoptium = "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot";
    if Path::new(adoptium).join("bin/java.exe").exists() {
        return Some(adoptium.to_string());
    }
    None
}

/// Resolve the FjpProbe classes directory — relative to the workspace
/// root (`vm/Cargo.toml` lives at `<repo>/vm`). Returns None if the
/// fixture has not been compiled (e.g., javac unavailable in CI), so
/// the test skips cleanly.
fn fjp_probe_classes() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let p = PathBuf::from(manifest_dir)
        .parent()
        .map(|p| p.join("apps").join("fjp_probe").join("classes"))?;
    if p.join("FjpProbe.class").exists() && p.join("FjpProbe$Sum.class").exists() {
        Some(p)
    } else {
        None
    }
}

/// RFJP.1 acceptance: spawn the freshly-built cratonvm in real-JDK mode
/// against `apps/fjp_probe/FjpProbe`, assert stdout contains
/// `sum = 499999500000` and `OK`.
///
/// CURRENTLY IGNORED — the fix is not yet shipped. Two prior agents
/// (Session 96 / Session 98) and this Session 108 attempt all hit
/// the same structural wall:
///   * The JIT mis-compiles recursive `compute()`-returning-Long at
///     depth >=10 (regalloc clobber on the self-call save/restore in
///     `jit/src/x64.rs::patch_self_calls` / `emit_invoke_virtual`),
///     so it returns 0 instead of the correct sum.
///   * Funneling FJP-family methods back into the interpreter via a
///     class-hierarchy gate (the obvious workaround) trades the bug
///     for an OS-level stack overflow: `RecursiveTask.join()` is a
///     Rust native that runs the child task by calling
///     `ctx.invoke_virtual(this, "compute", "()Ljava/lang/Object;")`,
///     which Rust-recurses through `invoke_or_native`. Without the
///     JIT short-circuit the recursion hits the 64MB main-vm stack
///     before the divide tree bottoms out.
///
/// Real fix needs ONE of: (a) fixing the regalloc clobber in
/// `emit_invoke_virtual`, or (b) re-plumbing `RecursiveTask.join()`
/// to push frames onto the JVM frame stack instead of Rust-recursing.
/// Track via roadmap RFJP.1.
// S108: actually fixed as a side-effect of CHM agent's Long.valueOf JIT
// miscompile fix. The recursive compute() returns Long; once Long.valueOf
// stopped JIT-miscompiling to value=0, the divide-and-conquer sum returns
// correctly. Both prior failure modes (regalloc clobber + interpreter
// stack overflow) were downstream symptoms of the boxing-zero bug.
#[test]
fn fjp_probe_recursive_returns_correct_sum() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "Skipping: cratonvm release binary not available at \
                 target/release/cratonvm.exe (build with `cargo build \
                 --release -p cratonvm-cli`)"
            );
            return;
        }
    };
    let java_home = match real_java_home() {
        Some(h) => h,
        None => {
            eprintln!(
                "Skipping: real JDK 25 not available (set JAVA_HOME or \
                 install Adoptium 25 at the documented path)"
            );
            return;
        }
    };
    let classes = match fjp_probe_classes() {
        Some(p) => p,
        None => {
            eprintln!(
                "Skipping: apps/fjp_probe/classes/FjpProbe.class missing \
                 (compile with `javac -d apps/fjp_probe/classes \
                 apps/fjp_probe/FjpProbe.java`)"
            );
            return;
        }
    };

    let output = Command::new(&bin)
        .args([
            "--java-home",
            &java_home,
            "-c",
            classes.to_str().unwrap(),
            "FjpProbe",
        ])
        .output()
        .expect("must spawn cratonvm.exe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "cratonvm exited non-zero. stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("sum = 499999500000"),
        "FjpProbe stdout missing `sum = 499999500000`. \
         The RFJP.1 workaround in `try_jit_compile_callee` / \
         `try_jit_upgrade_with_gate` may not be excluding ForkJoinTask \
         subclasses from JIT — the recursive Long compute() is hitting \
         the regalloc clobber and returning 0.\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "FjpProbe stdout missing `OK` marker. \
         stdout: {stdout}\nstderr: {stderr}"
    );
}
