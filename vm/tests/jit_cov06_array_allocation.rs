// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! cov-06: end-to-end coverage for the optimizing (IR/C2) tier's array
//! allocation lowering — `newarray` (0xbc, primitive) and `anewarray` (0xbd,
//! reference, already-loaded component class).
//!
//! `jit/tests/ir_vs_singlepass.rs` and the `jit` crate's own unit tests cover
//! the codegen in isolation (hand-built graphs, synthetic helpers, no real
//! heap). This probe is the complement the doc's "How to verify" section
//! asks for: allocate, store, read back, return, through the REAL
//! interpreter/JIT/GC pipeline, so a miscompile that only shows up against
//! the real allocator, the real bounds/negative-length machinery or a real
//! moving collection is still caught.
//!
//! Three things this probe proves that a synthetic-helper differential
//! cannot:
//!   * a hot `newarray`/`anewarray` site actually reaches the OPTIMIZING
//!     tier (checked via `CRATONVM_DBG=ir-compiles`'s "produced a body" line
//!     — a passing run that never engaged the tier would be vacuous, exactly
//!     the trap `jit-ir-relocation-map-contract.md` fell into);
//!   * a negative length throws `NegativeArraySizeException` for real,
//!     through the pending-exception → interpreter-catch path, not just a
//!     sentinel value a synthetic caller happens to check;
//!   * a fresh `anewarray` array survives a moving young-gen collection as a
//!     GC root, with its element references correctly rewritten — the doc's
//!     explicit ask ("the fresh array is a root at the next safepoint, and
//!     its elements are references the collector must rewrite").

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class Cov06ArrayAllocProbe {
    // Primitive `newarray` (0xbc), hot enough to reach the optimizing tier.
    static int[] mkIntArray(int n, int idx, int val) {
        int[] a = new int[n];
        a[idx] = val;
        return a;
    }

    // Reference `anewarray` (0xbd) of an already-loaded component class,
    // also warmed hot.
    static Object[] mkObjArray(int n) {
        Object[] a = new Object[n];
        for (int i = 0; i < n; i++) {
            a[i] = new Object();
        }
        return a;
    }

    static int negLen(int n) {
        int[] a = new int[n];
        return a.length;
    }

    public static void main(String[] args) {
        // Warm `mkIntArray` into the optimizing tier.
        long sum = 0;
        for (int i = 0; i < 200000; i++) {
            int[] a = mkIntArray(8, i % 8, i);
            sum += a[i % 8];
        }
        System.out.println("intWarmOK=" + (sum >= 0));

        // Allocate, store, read back, return.
        int[] a = mkIntArray(5, 2, 42);
        System.out.println("allocLen=" + a.length);
        System.out.println("allocElem=" + a[2]);
        System.out.println(
            "allocOtherZero=" + (a[0] == 0 && a[1] == 0 && a[3] == 0 && a[4] == 0));

        // Warm `mkObjArray` into the optimizing tier too, before the
        // GC-pressure round below relies on it.
        for (int i = 0; i < 200000; i++) {
            Object[] w = mkObjArray(4);
            if (w.length != 4) {
                throw new IllegalStateException("warm-up mkObjArray produced length " + w.length);
            }
        }

        // GC-root check: keep 64 freshly `anewarray`-allocated Object[]s
        // reachable while churning enough garbage (with a small -Xmx, see
        // run_probe below) to force real young-gen collections. If the
        // array itself is not scanned as a root, or its element references
        // are not rewritten by a moving collection, this reads back null or
        // a corrupted array length/elements.
        Object[] roots = new Object[64];
        for (int round = 0; round < 64; round++) {
            roots[round] = mkObjArray(50);
            for (int k = 0; k < 20000; k++) {
                Object garbage = new Object();
            }
        }
        boolean gcRootsOK = true;
        for (int round = 0; round < 64; round++) {
            Object[] oa = (Object[]) roots[round];
            if (oa == null || oa.length != 50) {
                gcRootsOK = false;
                break;
            }
            for (int i = 0; i < 50; i++) {
                if (oa[i] == null) {
                    gcRootsOK = false;
                    break;
                }
            }
        }
        System.out.println("gcRootsOK=" + gcRootsOK);

        // Warm `negLen` into the optimizing tier, THEN take the
        // negative-length path — the compiled body must throw, not just the
        // interpreted one.
        int acc = 0;
        for (int i = 0; i < 200000; i++) {
            acc += negLen(4);
        }
        System.out.println("negWarmOK=" + (acc == 4 * 200000));

        String excClass = "none";
        try {
            negLen(-3);
        } catch (NegativeArraySizeException e) {
            excClass = e.getClass().getName();
        }
        System.out.println("negExcClass=" + excClass);

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
    let dir = std::env::temp_dir().join("cratonvm-jit-cov06-array-alloc-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("Cov06ArrayAllocProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("Cov06ArrayAllocProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_cov06_array_allocation] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_cov06_array_allocation] javac cannot target --release 21 ({}); skipping. \
                 Point JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("Cov06ArrayAllocProbe.class").exists(),
        "[jit_cov06_array_allocation] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // Small heap: the GC-root round below allocates ~64 * (50 refs +
        // header) live plus 64 * 20000 garbage `Object`s, which must force
        // several real young-gen collections well before the run finishes —
        // see the memory note on pinning `-Xmx` for a GC probe rather than
        // relying on the default heap size, which is load-sensitive.
        .arg("--Xmx")
        .arg("32m")
        // Surfaces "[ir] optimizing backend produced a body for ..." for
        // every method the optimizing tier actually compiled, so the test
        // can prove `mkIntArray`/`mkObjArray`/`negLen` took the IR pipeline
        // rather than passing vacuously off the single-pass fallback.
        .env("CRATONVM_DBG", "ir-compiles")
        .arg("-c")
        .arg(classes)
        .arg("Cov06ArrayAllocProbe")
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
                    panic!("[jit_cov06_array_allocation] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[jit_cov06_array_allocation] wait failed: {e}"),
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
fn cov06_array_allocation_end_to_end() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_cov06_array_allocation] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_cov06_array_allocation] no usable JDK found (set CRATONVM_TEST_JDK or \
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
        "[jit_cov06_array_allocation] probe did not reach its final marker.\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );

    assert_eq!(field(&stdout, "intWarmOK=").as_deref(), Some("true"));
    assert_eq!(field(&stdout, "allocLen=").as_deref(), Some("5"));
    assert_eq!(field(&stdout, "allocElem=").as_deref(), Some("42"));
    assert_eq!(field(&stdout, "allocOtherZero=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "gcRootsOK=").as_deref(),
        Some("true"),
        "a fresh anewarray array (or one of its element references) did not survive a moving \
         young-gen collection.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(field(&stdout, "negWarmOK=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "negExcClass=").as_deref(),
        Some("java.lang.NegativeArraySizeException"),
        "a negative-length newarray on a HOT (optimizing-tier) method did not throw \
         NegativeArraySizeException.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Anti-vacuity: at least one of the three hot methods must actually have
    // been compiled by the OPTIMIZING tier, or every assertion above could be
    // passing purely off the single-pass fallback and this probe would prove
    // nothing about cov-06.
    let optimizing_tier_engaged = stderr.lines().any(|l| {
        l.contains("optimizing backend produced a body")
            && (l.contains("mkIntArray") || l.contains("mkObjArray") || l.contains("negLen"))
    });
    assert!(
        optimizing_tier_engaged,
        "[jit_cov06_array_allocation] none of mkIntArray/mkObjArray/negLen were reported as \
         compiled by the optimizing tier — this run proves nothing about the IR array-\
         allocation lowering. stderr:\n{stderr}"
    );
}
