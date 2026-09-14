// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: heap exhaustion inside a *native* allocation must raise a
//! catchable `java.lang.OutOfMemoryError`, not abort the process.
//!
//! `NativeContext::new_array` / `new_ref_array` / `alloc_object` funnel into
//! the panicking `GenerationalHeap::alloc_array` / `alloc_object`, which
//! `std::process::abort()` once young **and** old generation are both full.
//! That killed the whole VM — every other test sharing the process with it —
//! where HotSpot throws a recoverable `OutOfMemoryError`. Three members of the
//! family were fixed one call site at a time (`ArrayList(int)`,
//! the `(int)`-capacity constructors, `Cipher.doFinal`); the fourth,
//! `ByteBuffer.allocate(int)`, killed H2's `org.h2.test.db.TestOutOfMemory`
//! with `SIGABRT` after ~4 minutes.
//!
//! The fix is a scoped unwind channel (`vm::runtime::native_oom`): a native
//! callback dispatched by `safe_native_call` runs directly under that
//! function's `catch_unwind` with no JIT-compiled frame in between, so the
//! allocators unwind there on true double-exhaustion and the boundary converts
//! it into the catchable Java error.
//!
//! This test asserts the *observable* contract, three ways, and — critically —
//! that the VM is still usable afterwards:
//!
//! 1. one over-large `ByteBuffer.allocate` throws instead of aborting,
//! 2. allocating byte arrays until the heap is gone throws,
//! 3. allocating `ByteBuffer`s (the native path) until the heap is gone throws.
//!
//! Verified against HotSpot JDK 25: identical line-for-line output.
//!
//! Skips gracefully when `javac`, a JDK home, or the cratonvm binary are
//! unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

public class NativeOomProbe {
    static void bbHuge() {
        try {
            ByteBuffer b = ByteBuffer.allocate(Integer.MAX_VALUE - 8);
            System.out.println("1 BB_HUGE: NO_THROW cap=" + b.capacity());
        } catch (OutOfMemoryError e) {
            System.out.println("1 BB_HUGE: CAUGHT OutOfMemoryError");
        } catch (Throwable t) {
            System.out.println("1 BB_HUGE: CAUGHT " + t.getClass().getName());
        }
    }

    static void fillWithArrays() {
        List<byte[]> keep = new ArrayList<>();
        try {
            for (int i = 0; i < 100000; i++) {
                keep.add(new byte[4 * 1024 * 1024]);
            }
            System.out.println("2 ARR_FILL: NO_THROW");
        } catch (OutOfMemoryError e) {
            System.out.println("2 ARR_FILL: CAUGHT OutOfMemoryError");
        } catch (Throwable t) {
            System.out.println("2 ARR_FILL: CAUGHT " + t.getClass().getName());
        } finally {
            keep.clear();
        }
    }

    static void fillWithByteBuffers() {
        List<ByteBuffer> keep = new ArrayList<>();
        try {
            for (int i = 0; i < 100000; i++) {
                keep.add(ByteBuffer.allocate(4 * 1024 * 1024));
            }
            System.out.println("3 BB_FILL: NO_THROW");
        } catch (OutOfMemoryError e) {
            System.out.println("3 BB_FILL: CAUGHT OutOfMemoryError");
        } catch (Throwable t) {
            System.out.println("3 BB_FILL: CAUGHT " + t.getClass().getName());
        } finally {
            keep.clear();
        }
    }

    static void aliveCheck(String tag) {
        System.gc();
        ByteBuffer bb = ByteBuffer.allocate(4096);
        bb.putInt(0, 42);
        byte[] arr = new byte[4096];
        arr[10] = 7;
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 100; i++) {
            sb.append(i);
        }
        System.out.println(tag + " ALIVE: bb=" + bb.getInt(0) + " cap=" + bb.capacity()
                + " arr=" + arr[10] + " sb=" + sb.length());
    }

    public static void main(String[] a) {
        bbHuge();
        aliveCheck("1b");
        fillWithArrays();
        aliveCheck("2b");
        fillWithByteBuffers();
        aliveCheck("3b");
        System.out.println("DONE");
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
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
        "/home/victor/jdk25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-native-oom-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("NativeOomProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("NativeOomProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[native_alloc_oom_catchable] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[native_alloc_oom_catchable] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("NativeOomProbe.class").exists(),
        "[native_alloc_oom_catchable] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// Run the probe once with the given extra VM arguments and assert the full
/// contract.
fn run_probe(extra_args: &[&str], label: &str) {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[native_alloc_oom] cratonvm binary not found; build with \
                 `cargo build --release -p cratonvm-cli`. Skipping."
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[native_alloc_oom] no JDK home (set CRATONVM_TEST_JDK or \
                 JAVA_HOME); skipping"
            );
            return;
        }
    };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let classes = match compile_probe(&javac) {
        Some(d) => d,
        None => {
            eprintln!("[native_alloc_oom] javac unavailable; skipping");
            return;
        }
    };

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home").arg(&jdk);
    for a in extra_args {
        cmd.arg(a);
    }
    // A small heap keeps the fill loops short; the contract under test is
    // independent of the heap size.
    cmd.arg("--Xmx").arg("256m");
    cmd.arg("-c").arg(&classes).arg("NativeOomProbe");
    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[native_alloc_oom] failed to spawn cratonvm: {e}");
            return;
        }
    };
    let timeout = Duration::from_secs(300);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[native_alloc_oom/{label}] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[native_alloc_oom/{label}] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let ctx = format!("\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");

    // The regression itself: the panicking allocator's abort path.
    assert!(
        !stderr.contains("young gen exhausted"),
        "[native_alloc_oom/{label}] the panicking allocator's abort diagnostic \
         fired — heap exhaustion in a native still aborts the VM.{ctx}"
    );
    assert!(
        !stderr.contains("A fatal error has been detected"),
        "[native_alloc_oom/{label}] a crash report was written for the handled \
         OOM unwind.{ctx}"
    );

    for expected in [
        "1 BB_HUGE: CAUGHT OutOfMemoryError",
        "1b ALIVE: bb=42 cap=4096 arr=7 sb=190",
        "2 ARR_FILL: CAUGHT OutOfMemoryError",
        "2b ALIVE: bb=42 cap=4096 arr=7 sb=190",
        "3 BB_FILL: CAUGHT OutOfMemoryError",
        "3b ALIVE: bb=42 cap=4096 arr=7 sb=190",
        "DONE",
    ] {
        assert!(
            stdout.lines().any(|l| l.trim() == expected),
            "[native_alloc_oom/{label}] missing expected line `{expected}`.{ctx}"
        );
    }
}

#[test]
fn native_allocation_oom_is_catchable() {
    run_probe(&[], "jit");
}

#[test]
fn native_allocation_oom_is_catchable_nojit() {
    run_probe(&["--nojit"], "nojit");
}
