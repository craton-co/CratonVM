// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Virtual-thread probe regression test.
//!
//! Pins the runtime behavior of `Thread.ofVirtual().start(...)` end-to-end
//! through the `cratonvm.exe` CLI binary against the checked-in probes in
//! `vm/tests/resources/vthread_probe/`:
//!
//!   * `Counter.java`     — 1 virtual thread incrementing an `AtomicInteger`,
//!                          must print `After: n=1`. (Matches the user-supplied
//!                          repro for the "v-thread counter only reaches 1"
//!                          investigation: Counter.java is by-design 1 thread,
//!                          so `n=1` is the **correct** expected output.)
//!   * `VthreadProbe.java`— 10000 virtual threads sleeping 10 ms each then
//!                          incrementing a shared `AtomicInteger` and counting
//!                          down a `CountDownLatch`. Must print
//!                          `counted=10000 ok=true` followed by `OK`.
//!   * `Tiny.java`        — 1 virtual thread printing a line; pins
//!                          `Thread.ofVirtual()` builder + .start(Runnable) +
//!                          Thread.join() round-trip including the
//!                          `Joined OK` final line.
//!   * `VthreadGcStress.java` — 3000 virtual threads sleeping 5 ms while one
//!                          platform thread drives 400 `System.gc()` pauses.
//!                          The deterministic gate for the STW-arrival hole
//!                          fixed on 2026-09-05 (see `VTHREAD_PROBE_CAP`).
//!
//! The binary path is resolved via `CRATONVM_BIN` env var, then the cargo
//! `target/{release,debug}` fallback. Class files are produced on-demand via
//! `javac --release 21` if absent. If neither the binary nor `javac` is
//! available the test reports `skipped` rather than failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("tests")
        .join("resources")
        .join("vthread_probe")
}

fn probe_classes_dir() -> PathBuf {
    probe_dir().join("classes")
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

/// Compile the vthread probes via `javac` if the classes directory is missing
/// or stale relative to the .java source files. Best-effort.
fn ensure_probes_compiled() -> bool {
    let classes = probe_classes_dir();
    let required = [
        "Counter.class",
        "Tiny.class",
        "VthreadProbe.class",
        "VthreadGcStress.class",
    ];
    if required.iter().all(|f| classes.join(f).exists()) {
        return true;
    }
    let _ = std::fs::create_dir_all(&classes);
    let dir = probe_dir();
    let sources: Vec<PathBuf> = [
        "Counter.java",
        "Tiny.java",
        "VthreadProbe.java",
        "VthreadGcStress.java",
    ]
    .iter()
        .map(|f| dir.join(f))
        .filter(|p| p.exists())
        .collect();
    if sources.is_empty() {
        return false;
    }
    let mut cmd = Command::new("javac");
    cmd.arg("--release").arg("21").arg("-d").arg(&classes);
    for src in &sources {
        cmd.arg(src);
    }
    match cmd.output() {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            assert!(
                o.status.success(),
                "[vthread_probe_regression] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            required.iter().all(|f| classes.join(f).exists())
        }
    }
}

/// Run a single probe class through the cratonvm binary. Returns
/// `Some((stdout, stderr))` on successful spawn (regardless of exit code, so
/// callers can examine output even when the VM exits non-zero), `None` when
/// pre-requisites are unavailable so the caller can `return` and report skip.
fn run_probe(class_name: &str, timeout: Duration) -> Option<(String, String)> {
    if !ensure_probes_compiled() {
        eprintln!(
            "[vthread_probe_regression] vthread_probe class files unavailable; skipping {class_name}"
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[vthread_probe_regression] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let classes = probe_classes_dir();
    // Spawn a child process with the requested timeout. We use a thread-based
    // wait so we can kill the child if it hangs (helps when the v-thread
    // scheduler regresses to a 1-carrier livelock).
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&classes)
        .arg(class_name)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vthread_probe_regression] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    // DRAIN WHILE WAITING. The loop this replaces polled `try_wait` over piped
    // stdio without reading it, which deadlocks the moment a child outruns the
    // 64 KiB pipe — the child blocks in `write`, never exits, and the guard
    // reports a "timeout" for a process that finished its work. That is not
    // what is failing here today (this probe writes 175 bytes), but it is the
    // same latent defect that cost `native_io_dis_read_fully_pin` 10 timeouts
    // in 10 on Linux, and the next diagnostic anyone adds to the VM re-creates
    // it. `common::wait_draining` reads both pipes on their own threads and
    // returns what it captured even when the cap is hit.
    let timed = common::wait_draining(child, timeout);
    let stdout = String::from_utf8_lossy(&timed.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&timed.output.stderr).into_owned();
    if timed.timed_out {
        panic!(
            "[vthread_probe_regression] {class_name} timed out after {timeout:?}
             stdout:
{stdout}
stderr:
{stderr}"
        );
    }
    Some((stdout, stderr))
}

/// The hang cap for `VthreadProbe`. It is a LIVELOCK GUARD, not a performance
/// assertion: the test asserts `counted=10000 ok=true`, and this only exists so
/// a scheduler that stops making progress fails fast instead of hanging the
/// suite.
///
/// # It was briefly 300 s, and that was wrong
///
/// `vthread_probe_10000_all_increment` flaked in the 2026-09-05 Linux sweep
/// (2 failures in 8). Twenty runs of the probe under added load looked like a
/// heavy tail — 20/20 correct, 3.36 s to 26.39 s — so the cap was raised to
/// 300 s on the theory that 60 s sat inside that tail.
///
/// THE TAIL WAS NOT THE STORY. Running the VM DIRECTLY, no test harness
/// involved, ten times on Windows:
///
/// ```text
/// 8 runs   2-4 s   counted=10000 ok=true
/// 2 runs   killed at 120 s, no output at all
/// ```
///
/// Bimodal, with nothing in between, and independent of machine load — one
/// failure came with three background compilers running and three of the
/// passes came with the same three. That is a HANG, and a cap cannot fix a
/// hang: raising it to 300 s bought nothing except making CI wait five times
/// longer to report a real defect, so it is back to 60 s — twenty times the
/// healthy runtime and twice the worst completed run ever measured.
///
/// # What the hang was (FIXED 2026-09-05)
///
/// Not the carrier pool, and not the scheduler. A virtual thread that yields
/// (`Thread.sleep` -> `ContinuationYield`) deposited its root snapshot — which
/// raises `in_blocked_region` and excludes it from FUTURE pauses — and then
/// handed itself to `suspend_runtime` WITHOUT arriving for a pause that was
/// already in flight and had already counted it in `expected`. The carrier
/// went back to `wait_for_task_until`, nothing on that OS thread ever arrived
/// for that `tid` again, and `wait_for_all` blocked forever. See
/// `vthread-probe-intermittent-hang-FIXED-20260905.md`.
///
/// This test remains a one-in-five detector for that defect, which is not a
/// gate — `vthread_gc_stress_completes` below is the deterministic one.
const VTHREAD_PROBE_CAP: Duration = Duration::from_secs(60);

/// Cap for `VthreadGcStress`. Healthy runs finish in 7-22 s on a loaded
/// 8-core host; the pre-fix binary hung 8 times out of 8 and was still hung at
/// 60 s every time, so 120 s is a generous livelock guard rather than a
/// performance assertion.
const VTHREAD_GC_STRESS_CAP: Duration = Duration::from_secs(120);

/// Memoize each probe run so all subtests targeting the same class share one
/// VM spawn. Keyed by class name.
fn cached_run(class_name: &'static str, timeout: Duration) -> Option<(String, String)> {
    static COUNTER: OnceLock<Option<(String, String)>> = OnceLock::new();
    static TINY: OnceLock<Option<(String, String)>> = OnceLock::new();
    static VTHREAD: OnceLock<Option<(String, String)>> = OnceLock::new();
    static GC_STRESS: OnceLock<Option<(String, String)>> = OnceLock::new();
    let cell: &OnceLock<Option<(String, String)>> = match class_name {
        "Counter" => &COUNTER,
        "Tiny" => &TINY,
        "VthreadProbe" => &VTHREAD,
        "VthreadGcStress" => &GC_STRESS,
        other => panic!("unknown vthread probe class: {other}"),
    };
    cell.get_or_init(|| run_probe(class_name, timeout)).clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `Counter.java` launches **one** virtual thread that increments an
/// `AtomicInteger`. The expected output is `After: n=1`. This pins the
/// minimal `Thread.ofVirtual().start(Runnable)` round-trip on the carrier
/// pool so a regression to "vthread launched but never executes" surfaces
/// as `n=0` rather than an unrelated timeout/crash.
#[test]
fn vthread_counter_single_increments_to_1() {
    let (stdout, stderr) = match cached_run("Counter", Duration::from_secs(30)) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("Begin"),
        "Counter probe never reached the 'Begin' line — VM bootstrap regressed?\n{combined}"
    );
    assert!(
        combined.contains("After: n=1"),
        "Counter probe expected 'After: n=1' but got:\n{combined}"
    );
    // Defensive: explicitly reject obviously broken outputs.
    assert!(
        !combined.contains("After: n=0"),
        "Counter probe printed 'After: n=0' — vthread spawned but never \
         executed the Runnable body before main joined.\n{combined}"
    );
}

/// `Tiny.java` exercises `Thread.ofVirtual()` builder + `.start(Runnable)` +
/// `Thread.join()`. Pins that the builder is reachable, that the v-thread
/// runs to completion (printing `In vthread`), and that join returns cleanly
/// (`Joined OK`).
#[test]
fn vthread_tiny_builder_start_join() {
    let (stdout, stderr) = match cached_run("Tiny", Duration::from_secs(30)) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    for needle in ["Begin", "Got builder:", "In vthread", "Joined OK"] {
        assert!(
            combined.contains(needle),
            "Tiny probe missing expected line '{needle}'. Output:\n{combined}"
        );
    }
}

/// `VthreadProbe.java` launches 10000 virtual threads each sleeping 10 ms
/// then incrementing a shared `AtomicInteger` and counting down a
/// `CountDownLatch`. The acceptance gate is the literal `counted=10000`
/// line followed by `OK` on stdout. This is the load-bearing regression
/// test against "only one carrier executes" / "carrier pool starves" /
/// "Thread.sleep on a v-thread never resumes".
#[test]
fn vthread_probe_10000_all_increment() {
    let (stdout, stderr) = match cached_run("VthreadProbe", VTHREAD_PROBE_CAP) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    if !combined.contains("counted=10000 ok=true") {
        // Surface the actual count line for triage so a regression to e.g.
        // `counted=1 ok=false` or `counted=4096 ok=false` is immediately
        // visible.
        let counted_line = combined
            .lines()
            .find(|l| l.starts_with("counted="))
            .unwrap_or("(no `counted=` line emitted)");
        panic!(
            "VthreadProbe failed to count all 10000 vthreads.\n\
             observed: {counted_line}\n\
             full output:\n{combined}"
        );
    }
    assert!(
        combined.contains("\nOK\n")
            || combined.trim_end().ends_with("OK")
            || combined.contains("\nOK\r\n"),
        "VthreadProbe printed counted=10000 but never reached final 'OK' \
         marker — exit raced the println? Output:\n{combined}"
    );
    assert!(
        !combined.contains("FAIL"),
        "VthreadProbe explicitly printed FAIL:\n{combined}"
    );
}

/// `VthreadGcStress.java` — the deterministic gate for the stop-the-world
/// arrival hole on the virtual-thread YIELD path (fixed 2026-09-05).
///
/// 3000 virtual threads each sleep 5 ms while one platform thread calls
/// `System.gc()` 400 times, so a pause is nearly always in flight at the
/// instant a continuation unmounts. That is exactly the window in which the
/// unfixed VM counted a continuation in a pause's `expected` quota and then
/// let it become a heap-resident continuation that could never arrive.
///
/// Measured on Azure `20.80.105.49`, `dev` tip `a044e1fe1`: **8 hangs in 8**
/// at a 60 s cap before the fix, **8 clean in 8** (7-22 s) after it. The
/// `VthreadProbe` test above catches the same defect about one run in five,
/// which is why this fixture exists.
#[test]
fn vthread_gc_stress_completes() {
    let (stdout, stderr) = match cached_run("VthreadGcStress", VTHREAD_GC_STRESS_CAP) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}
--- STDERR ---
{stderr}");
    if !combined.contains("counted=3000 ok=true") {
        let counted_line = combined
            .lines()
            .find(|l| l.starts_with("counted="))
            .unwrap_or("(no `counted=` line emitted)");
        panic!(
            "VthreadGcStress failed to count all 3000 vthreads.
             observed: {counted_line}
             full output:
{combined}"
        );
    }
    assert!(
        combined.contains("OK"),
        "VthreadGcStress printed counted=3000 but never reached the final          'OK' marker. Output:
{combined}"
    );
}
