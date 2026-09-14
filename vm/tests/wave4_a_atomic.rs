// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 4 / Task A — `java.util.concurrent.atomic` CAS regression.
//!
//! Pins the runtime behavior of `AtomicInteger` / `AtomicLong` /
//! `AtomicReference` `compareAndSet` (and the 8-thread contended-loop
//! pattern that ConcurrentHashMap, AQS, lock-free queues etc. depend on)
//! end-to-end through the `cratonvm.exe` CLI binary against
//! `apps/atomic_probe/AtomicProbe.java`.
//!
//! Acceptance criteria (all in a single subprocess run):
//!   * `ai.cas1=true cas2=false val=1`        — sequential AtomicInteger CAS
//!   * `al.cas=true val=100`                  — sequential AtomicLong CAS
//!   * `ar.rcas1=true rcas2=false val=world`  — reference-identity CAS
//!     (rcas1 swaps `s1`→`s2`; rcas2 expects a *different* `new String("hello")`
//!     and so must fail by identity — value remains `s2 == "world"`)
//!   * `contended.final=800000 expected=800000` — 8 threads × 100 000 CAS
//!     loop increments must commit every store with no lost updates
//!   * Final `OK` marker
//!
//! The contention test is the real regression pin: a non-atomic
//! `Unsafe.compareAndSet*` would surface here as `final < 800000` (lost
//! updates) or as a livelock / deadlock (subprocess timeout). 30 s subprocess
//! timeout — HotSpot finishes in ~0.3 s, cratonvm release in ~5 s.
//!
//! The binary path is resolved via `CRATONVM_BIN` env var, then the cargo
//! `target/{release,debug}` fallback. Class files are produced on-demand via
//! `javac --release 21` if absent. If neither the binary nor `javac` is
//! available the test reports `skipped` rather than failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().join("apps").join("atomic_probe")
}

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
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
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

/// True when `class_file` exists and is at least as new as `src`.
///
/// A `.class` older than its `.java` is a standing trap here: reusing it means
/// the run exercises a stale fixture, so a landed source change shows up in no
/// log. When the mtimes cannot prove freshness, recompile.
fn up_to_date(class_file: &Path, src: &Path) -> bool {
    let (Ok(c), Ok(s)) = (class_file.metadata(), src.metadata()) else {
        return false;
    };
    match (c.modified(), s.modified()) {
        (Ok(c), Ok(s)) => c >= s,
        // No mtime on this filesystem: recompile rather than trust a stale class.
        _ => false,
    }
}

/// Compile `AtomicProbe.java` via `javac` if `AtomicProbe.class` is missing or
/// older than the source. Best-effort: returns false if javac is unavailable or
/// compilation fails.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("AtomicProbe.class");
    let source = dir.join("AtomicProbe.java");
    if !source.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave4_a",
            "the Wave 4 Task A fixture `AtomicProbe.java`",
            &[source.clone()],
        );
        return false;
    }
    if up_to_date(&class_file, &source) {
        return true;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[wave4_a_atomic] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wave4_a_atomic] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// Run `AtomicProbe` through the cratonvm binary with a hard timeout.
/// Returns `Some((stdout, stderr))` on successful spawn, `None` if
/// pre-requisites are missing (so the caller can `return` and skip).
fn run_atomic_probe(timeout: Duration) -> Option<(String, String)> {
    if !ensure_probe_compiled() {
        eprintln!(
            "[wave4_a_atomic] AtomicProbe.class unavailable (javac on PATH?); \
             skipping"
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[wave4_a_atomic] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let dir = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&dir)
        .arg("AtomicProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave4_a_atomic] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[wave4_a_atomic] AtomicProbe timed out after {timeout:?} \
                         — likely CAS livelock or atomicity regression"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave4_a_atomic] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave4_a_atomic] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// One subprocess run shared across all 5 sub-tests so we pay the VM
/// bootstrap cost (and the 8-thread × 100 000 contention loop) only once.
fn cached_run() -> Option<(String, String)> {
    static CELL: OnceLock<Option<(String, String)>> = OnceLock::new();
    CELL.get_or_init(|| run_atomic_probe(Duration::from_secs(30)))
        .clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `AtomicInteger.compareAndSet` sequential identity:
///   * `cas1 = ai.compareAndSet(0, 1)` succeeds (true) — value becomes 1
///   * `cas2 = ai.compareAndSet(0, 2)` fails (false)   — value stays 1
#[test]
fn atomic_integer_sequential_cas() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("ai.cas1=true cas2=false val=1"),
        "AtomicInteger sequential CAS regressed. Expected \
         'ai.cas1=true cas2=false val=1' in:\n{combined}"
    );
}

/// `AtomicLong.compareAndSet(0L, 100L)` succeeds; final value is 100.
/// Distinct from the int path because long values traverse the
/// `Value::Long` descriptor branch in `compare_and_swap_field`.
#[test]
fn atomic_long_sequential_cas() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("al.cas=true val=100"),
        "AtomicLong sequential CAS regressed. Expected \
         'al.cas=true val=100' in:\n{combined}"
    );
}

/// `AtomicReference.compareAndSet` must use **reference identity** (`==`),
/// not `equals()`. Probe sets `ar` to `s1`, swaps to `s2` (rcas1=true),
/// then attempts to swap with a *fresh* `new String("hello")` (rcas2=false:
/// equals s1, not == s1). Final value remains `s2 == "world"`.
#[test]
fn atomic_reference_identity_cas() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("ar.rcas1=true rcas2=false val=world"),
        "AtomicReference identity CAS regressed. Expected \
         'ar.rcas1=true rcas2=false val=world' (rcas2 must compare by \
         reference identity, not equals) in:\n{combined}"
    );
}

/// 8 threads × 100 000 iterations of a `do { old=get(); } while
/// (!compareAndSet(old, old+1));` loop must commit every store. A
/// non-atomic CAS surfaces as `contended.final < 800000`; a livelock
/// surfaces as the 30 s subprocess timeout.
///
/// This is the load-bearing regression test for ConcurrentHashMap,
/// AbstractQueuedSynchronizer, and every JDK lock-free data structure.
#[test]
fn atomic_8_thread_contention_no_lost_updates() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    if !combined.contains("contended.final=800000 expected=800000") {
        let observed = combined
            .lines()
            .find(|l| l.starts_with("contended.final="))
            .unwrap_or("(no `contended.final=` line emitted)");
        panic!(
            "AtomicInteger 8-thread contention regression — lost CAS updates.\n\
             observed: {observed}\n\
             full output:\n{combined}"
        );
    }
}

/// Final `OK` marker — guards against the probe printing all expected
/// values then crashing or hanging in `Thread.join`.
#[test]
fn atomic_probe_final_ok_marker() {
    let (stdout, stderr) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("\nOK\n")
            || combined.trim_end().ends_with("OK")
            || combined.contains("\nOK\r\n"),
        "AtomicProbe never reached final 'OK' — main() exited \
         abnormally?\n{combined}"
    );
}
