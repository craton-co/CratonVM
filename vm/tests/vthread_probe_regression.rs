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
///
/// # The staleness half of that sentence was a claim, not a check
///
/// This function's contract has said "or stale" since it was written, and the
/// test was `required.iter().all(|f| classes.join(f).exists())` — existence
/// only. `classes/` is CHECKED IN, so editing a probe's `.java` and running
/// the test re-ran the old `.class` and reported on source that was no longer
/// in the tree. Found 2026-09-11 while adding the progress heartbeats these
/// probes now print: the new `.java` files were in place, the test was green,
/// and not one heartbeat appeared.
///
/// It is the same family as `common::warn_if_stale`, one level down — that one
/// catches a launcher older than the sources, this one a FIXTURE older than
/// its own source — and the same failure mode: a green that is about something
/// other than what you changed.
fn ensure_probes_compiled() -> bool {
    let classes = probe_classes_dir();
    let required = [
        ("Counter.class", "Counter.java"),
        ("Tiny.class", "Tiny.java"),
        ("VthreadProbe.class", "VthreadProbe.java"),
        ("VthreadGcStress.class", "VthreadGcStress.java"),
    ];
    let dir = probe_dir();
    let fresh = |class: &str, java: &str| -> bool {
        let (Ok(c), Ok(j)) = (
            std::fs::metadata(classes.join(class)).and_then(|m| m.modified()),
            std::fs::metadata(dir.join(java)).and_then(|m| m.modified()),
        ) else {
            // No `.class` at all, or no `.java` to compare against. The first
            // means compile; the second is a broken checkout the compile step
            // below reports far better than a silent `true` would.
            return false;
        };
        c >= j
    };
    if required.iter().all(|(c, j)| fresh(c, j)) {
        return true;
    }
    let _ = std::fs::create_dir_all(&classes);
    let sources: Vec<PathBuf> = required
        .iter()
        .map(|(_, java)| dir.join(java))
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
            required.iter().all(|(c, _)| classes.join(c).exists())
        }
    }
}

/// Run a single probe class through the cratonvm binary. Returns
/// `Some((stdout, stderr))` on successful spawn (regardless of exit code, so
/// callers can examine output even when the VM exits non-zero), `None` when
/// pre-requisites are unavailable so the caller can `return` and report skip.
fn run_probe(class_name: &str, guard: Guard) -> Option<(String, String)> {
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
    let mut cmd = Command::new(&bin);
    cmd.arg("-c").arg(&classes).arg(class_name);
    if class_name == "VthreadGcStress" {
        let (threads, rounds) = gc_stress_workload();
        // `[threads] [gcRounds] [gcSleepMillis]`, and the sleep stays 1 ms:
        // it is what keeps a pause nearly always in flight.
        cmd.arg(threads.to_string())
            .arg(rounds.to_string())
            .arg("1");
    }
    let mut child = match cmd
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
    let watched = match guard {
        Guard::Cap(cap) => {
            let timed = common::wait_draining(child, cap);
            let stdout = String::from_utf8_lossy(&timed.output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&timed.output.stderr).into_owned();
            assert!(
                !timed.timed_out,
                "[vthread_probe_regression] {class_name} timed out after {cap:?}. This probe \
                 runs ONE virtual thread and prints one line, so it has no progress to report \
                 and is guarded by a plain cap; the two big probes are not. stdout:\n{stdout}\n\
                 stderr:\n{stderr}"
            );
            return Some((stdout, stderr));
        }
        Guard::Countdown { stall, ceiling } => common::wait_watching(
            child,
            common::Progress {
                countdown_key: "remaining=",
                stall,
                ceiling,
            },
        ),
    };
    assert!(
        watched.stop == common::Stop::Exited,
        "{}",
        watched.diagnosis(&format!("vthread_probe_regression {class_name}"))
    );
    let stdout = String::from_utf8_lossy(&watched.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&watched.output.stderr).into_owned();
    Some((stdout, stderr))
}

/// The `VthreadGcStress` workload: thread count and `System.gc()` rounds.
///
/// Keyed on the profile of the **binary under test**, not on
/// `cfg!(debug_assertions)` — that describes the harness, and the harness is
/// not what runs for 40 minutes.
///
/// # Why the debug run is smaller, and what that costs
///
/// 3000 threads x 400 rounds is the documented gate: "8 hangs in 8 at a 60 s
/// cap before the fix, 8 clean in 8 (7-22 s) after it", measured against a
/// RELEASE binary. `ci.yml` runs `cargo test --workspace`, which is debug, and
/// nobody had ever watched that combination finish — `registrar_drift` was red
/// upstream of it, and `cargo test` stops at the first failing binary.
///
/// It does not finish. Measured 2026-09-11 on an 8-core host, debug binary,
/// `remaining=` heartbeats watched throughout:
///
/// | threads x rounds | wall | user |
/// |---|---|---|
/// | 300 x 400 | 162 s | 141 s |
/// | 400 x 400 | 170 s | 161 s |
/// | 750 x 400 | 305 s | 283 s |
/// | 1500 x 400 | 319 s | 304 s |
/// | **1500 x 200** | **168 s / 170 s** | 155 s / 156 s |
/// | **3000 x 400** | **killed at 2400 s**, 231 of 400 rounds done | — |
///
/// `user` tracks `real` in every row, so this is compute and not contention:
/// a debug pause costs ~0.4 s (the ~150 s floor every row pays is 400 of
/// them), and the per-pause cost grows with the number of live continuations
/// until 3000 of them stops finishing altogether. Release does the whole thing
/// in 16-23 s.
///
/// So the release configuration stays exactly what it was and the debug one is
/// **1500 x 200** — still 200 stop-the-world pauses against 1500 live
/// continuations, which is the shape of the defect, at a fifteenth of the
/// thread-pause product. What it gives up is the *deterministic* claim: 8-in-8
/// was measured at 3000 x 400 and nobody has measured the reduced
/// configuration against a pre-fix binary, because the pre-fix binary is 60
/// commits back. A debug run that detects the hang some of the time is worth
/// having; a debug run nobody can afford to wait for is not, and that is what
/// was there.
fn gc_stress_workload() -> (u32, u32) {
    // An explicit `CRATONVM_BIN` is a deliberate act and may point anywhere,
    // so an unrecognisable path gets the full gate rather than the concession.
    let debug_build = cratonvm_binary_lookup().is_some_and(|p| {
        p.parent()
            .and_then(|d| d.file_name())
            .is_some_and(|n| n == "debug")
    });
    if debug_build {
        (1500, 200)
    } else {
        (3000, 400)
    }
}

/// How a probe run is guarded.
///
/// # The two caps this replaces, and why neither could work
///
/// ```text
/// const VTHREAD_PROBE_CAP: Duration = Duration::from_secs(60);
/// const VTHREAD_GC_STRESS_CAP: Duration = Duration::from_secs(120);
/// ```
///
/// Both said, at length and correctly, that they were LIVELOCK GUARDS and not
/// performance assertions. Neither could be one. A deadline cannot express
/// "stopped making progress", only "took too long", and those two came apart
/// in three separate ways:
///
/// * **Profile.** `VTHREAD_PROBE_CAP`'s derivation — "twenty times the healthy
///   runtime and twice the worst completed run ever measured" — rests on "8
///   runs 2-4 s" taken "running the VM DIRECTLY", which was a **release**
///   binary. `ci.yml` line 252 runs `cargo test --workspace`, which is debug.
///   Measured 2026-09-11 on one 8-core host: `VthreadProbe` 6.3 s release
///   against 30-125 s debug, `VthreadGcStress` 16-23 s release against 400 s+
///   debug. 60 s was 2x the healthy debug runtime, not 20x.
/// * **Load.** The same binary on the same host at load 40 took 52 s where it
///   had taken 30 s. `user` was 19 s of a 52 s wall clock: the probe was not
///   working harder, it was waiting for a core.
/// * **A debug-only tripwire, which turned out NOT to be the cost.**
///   `thread_state::stress_checks_enabled()` defaults to
///   `cfg!(debug_assertions)`, and its model is per-OS-THREAD while the states
///   it tracks belong to a LOGICAL thread, so a carrier reported every virtual
///   thread that died on it — 9 992 `illegal thread-state transition` lines,
///   4.2 MB of `tracing::error!`, on a probe whose real output is 759 bytes.
///   It is fixed (`thread_state::is_legal`) and it is recorded here because it
///   LOOKED like the explanation and is not: three paired runs came in at
///   46/179/46 s with the checks off against 22/23/74 s with them on. The
///   spread is host load, in both arms, and it swamps everything else.
///
/// Every one of those makes a HEALTHY run fail, which is the one thing a
/// livelock guard must never do — and the obvious repair, raising the
/// constant, is exactly the change that hides the hang the guard exists for.
/// `VTHREAD_PROBE_CAP` had already been to 300 s and back on 2026-09-05 for
/// that reason.
///
/// So the probes print a counter that only goes down and the guard watches the
/// COUNTER. See `common::Progress`.
enum Guard {
    /// A plain wall-clock cap, for a probe that runs one virtual thread and
    /// prints one line. There is nothing to watch, and nothing that takes
    /// long enough for the profile to matter.
    Cap(Duration),
    /// Watch `remaining=` and fail when it stops moving.
    Countdown { stall: Duration, ceiling: Duration },
}

/// How long a working vthread probe may go without advancing `remaining=`
/// once.
///
/// This is the one number the gate now depends on, and it is a **no-progress**
/// budget rather than a runtime one: it does not move when the workload, the
/// profile or the host does, because a probe that is working advances its
/// counter whatever else is true.
///
/// # What it had to be sized against, which was not what was expected
///
/// The probes heartbeat every 1/200th of their thread count through the spawn
/// loop and once a second afterwards, so the expected gap is a second or two —
/// and on a quiet host (load 11-13) that is what four runs showed, worst gap
/// 12.5 s. On the same host at load 33-42, two runs out of four stalled for
/// **146.9 s and 148.0 s** in the middle of the spawn loop, and then finished
/// correctly (166.8 s and 166.7 s wall, `counted=10000 ok=true`).
///
/// Those two stalls are the interesting measurement in this whole page:
///
/// * The process was **burning CPU** throughout — `main-vm` in state `R`,
///   ~0.8 cores for the whole 148 s, one carrier busy and the other seven
///   parked. Sampled live from `/proc`.
/// * `user` tracked `real` for the run as a whole (136 s and 147 s of 167 s).
/// * Both runs produced the right answer.
///
/// So it is not the 2026-09-05 deadlock, which used no CPU at all and never
/// finished; and it is not starvation, which does not look like 0.8 cores of
/// `R`. It is a long compute phase — an unoptimised collector or compiler
/// doing work whose cost the release build hides — that a Java-side progress
/// signal cannot report on, because whatever holds the world holds the thread
/// that would print the heartbeat. **A budget of this shape must exceed the
/// longest such phase, and 148 s is the longest one measured.**
///
/// 600 s is four times that. It is a large number and it buys the right thing:
/// a genuine freeze is reported 600 s after the last heartbeat rather than
/// 60 s after process start, and in exchange no healthy run can fail however
/// slow the profile or however busy the host. The 60 s cap this replaces had
/// already failed healthy runs twice, and been raised and lowered once.
const VTHREAD_STALL: Duration = Duration::from_secs(600);

/// Absolute backstop for a probe that keeps advancing and never finishes — a
/// different defect from the one the stall clock watches for, and one nobody
/// has seen here.
///
/// Deliberately far above any measured run (the worst completed run was
/// 179 s), because a ceiling that competes with the stall clock re-introduces
/// the bug this whole block is about. If it ever fires, the diagnosis says the
/// counter WAS moving, which is the fact worth having.
const VTHREAD_CEILING: Duration = Duration::from_secs(1800);

/// Memoize each probe run so all subtests targeting the same class share one
/// VM spawn. Keyed by class name.
fn cached_run(class_name: &'static str, guard: Guard) -> Option<(String, String)> {
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
    cell.get_or_init(|| run_probe(class_name, guard)).clone()
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
    let (stdout, stderr) = match cached_run("Counter", Guard::Cap(Duration::from_secs(30))) {
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
    let (stdout, stderr) = match cached_run("Tiny", Guard::Cap(Duration::from_secs(30))) {
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
    let (stdout, stderr) = match cached_run(
        "VthreadProbe",
        Guard::Countdown {
            stall: VTHREAD_STALL,
            ceiling: VTHREAD_CEILING,
        },
    ) {
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
    let (stdout, stderr) = match cached_run(
        "VthreadGcStress",
        Guard::Countdown {
            stall: VTHREAD_STALL,
            ceiling: VTHREAD_CEILING,
        },
    ) {
        Some(o) => o,
        None => return,
    };
    let combined = format!(
        "{stdout}
--- STDERR ---
{stderr}"
    );
    let (threads, rounds) = gc_stress_workload();
    let expected = format!("counted={threads} ok=true");
    if !combined.contains(&expected) {
        let counted_line = combined
            .lines()
            .find(|l| l.starts_with("counted="))
            .unwrap_or("(no `counted=` line emitted)");
        panic!(
            "VthreadGcStress failed to count all {threads} vthreads              ({rounds} System.gc() rounds -- see `gc_stress_workload`).
             observed: {counted_line}
             full output:
{combined}"
        );
    }
    assert!(
        combined.contains("OK"),
        "VthreadGcStress printed {expected} but never reached the final 'OK'          marker. Output:
{combined}"
    );
}
