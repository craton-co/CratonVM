// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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



fn field(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}


