// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared prerequisite reporting for the `vm/tests` integration suite.
//!
//! Most tests here drive a real `cratonvm` binary, and a few also need a real
//! JDK. When either is absent the historical behaviour is to `eprintln!` a note
//! and `return` from the test body — at which point cargo reports
//! `test ... ok`, **indistinguishable from a real pass**. A whole suite can
//! report green while executing none of its assertions.
//!
//! That is not hypothetical. On 2026-08-04 a `cargo build -p cratonvm-cli` was
//! cut off mid-link, and `jit_ir_athrow_dispatch` and
//! `jit_ir_exception_stub_throw_bci` both reported
//! `ok ... finished in 0.00s` while providing zero coverage. The only tell was
//! the duration — the same two tests take ~3s and ~4s when they actually run.
//!
//! # The switch
//!
//! Setting [`REQUIRE_VAR`] (`CRATONVM_REQUIRE_E2E=1`) turns every such skip
//! into a panic naming what was missing. **Unset — the default — behaviour is
//! byte-for-byte what it always was**, so a contributor with no JDK or no build
//! is never blocked. CI sets it to assert that a green run was a real one.
//!
//! `0` and the empty string read as unset, so `CRATONVM_REQUIRE_E2E=0` is a
//! usable off-switch rather than a surprising on-switch.
//!
//! # What this deliberately does NOT do
//!
//! It does not unify how a binary or a JDK is *located*. Those 78 lookups are
//! genuinely different (manifest-relative, workspace-relative, worktree-root,
//! `CRATONVM_BIN`-only, differing profile probe order), and collapsing them
//! would change *which* binary a test resolves — a behaviour change wearing a
//! refactor's clothes. Each test keeps its own lookup; only the report of a
//! MISSING prerequisite is shared. The wrappers are applied by post-composing
//! each file's original lookup, which is why no call site moved.

#![allow(dead_code)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Output};
use std::time::{Duration, Instant};

/// Environment variable that promotes a skipped prerequisite to a failure.
pub const REQUIRE_VAR: &str = "CRATONVM_REQUIRE_E2E";

/// True when the caller has demanded that prerequisites actually be present.
///
/// Unset, empty, and `0` all read as "not demanded" — the historical skip.
pub fn require_e2e() -> bool {
    match std::env::var(REQUIRE_VAR) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

/// Gate a `cratonvm` binary lookup: pass the result through unchanged, unless
/// it is `None` and [`REQUIRE_VAR`] is set — then fail loudly instead of
/// letting the caller skip to a green.
pub fn require_binary(found: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = found.as_deref() {
        warn_if_stale(path);
    }
    if found.is_none() && require_e2e() {
        panic!(
            "{REQUIRE_VAR} is set, but no `cratonvm` binary was found, so this test would have \
             skipped and still reported `ok`. Build one with `cargo build --release -p \
             cratonvm-cli`, or point `CRATONVM_BIN` at an existing binary. Unset {REQUIRE_VAR} \
             to go back to skipping."
        );
    }
    found
}

/// The newest modification time across the workspace's Rust sources and
/// manifests, computed once per test process.
///
/// Deliberately a SOURCE timestamp and not the test binary's own: a contributor
/// who runs `cargo build --release -p cratonvm-cli` and only later `cargo test`
/// has a launcher that is older than the test binary and perfectly current.
/// Comparing against the sources answers the question actually being asked —
/// "was this launcher built from at least this source state?" — and gives no
/// false alarm for that ordering.
///
/// `target/`, `.git/` and `.claude/` are skipped: the first is where the
/// artefacts being judged live, and including it would make every binary newer
/// than its own yardstick.
fn newest_source_mtime() -> Option<std::time::SystemTime> {
    use std::sync::OnceLock;
    static NEWEST: OnceLock<Option<std::time::SystemTime>> = OnceLock::new();
    *NEWEST.get_or_init(|| {
        fn walk(dir: &std::path::Path, newest: &mut Option<std::time::SystemTime>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name == "target" || name == ".git" || name == ".claude" {
                        continue;
                    }
                    walk(&path, newest);
                } else {
                    let interesting = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e == "rs")
                        || path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml");
                    if !interesting {
                        continue;
                    }
                    if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                        if newest.is_none_or(|n| m > n) {
                            *newest = Some(m);
                        }
                    }
                }
            }
        }
        // `vm/tests/common` -> workspace root.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
        let mut newest = None;
        walk(root, &mut newest);
        newest
    })
}

/// **IS THIS LAUNCHER OLDER THAN THE SOURCE THE TEST WAS BUILT FROM?**
///
/// The lookups in this suite prefer `target/release/cratonvm` over `debug` and
/// take whichever exists, with no freshness check at all — so a test can
/// silently measure a binary from an arbitrary earlier commit. That is the same
/// vacuous-result family this module's header is about, with both polarities
/// live: a fixed defect reported as OPEN, or, worse, an open defect reported as
/// FIXED because the binary predates the regression.
///
/// It is not hypothetical either. On 2026-09-05
/// `inet_address_hostname_contract` reported the ServerSocket wildcard defect
/// as still open six minutes after its fix merged, because
/// `target/release/cratonvm.exe` was built at 09:48 and the fix landed at
/// 09:54. Rebuilding the launcher turned the same test green with no source
/// change. Two other investigations the same day were sent down blind alleys by
/// stale binaries on a shared host.
///
/// `CRATONVM_BIN` is EXEMPT: naming a binary explicitly is a deliberate act,
/// and the H2 and Netty runners do it precisely to test an older build.
///
/// Warns always so the note is in front of anyone reading a failure, and fails
/// hard under `CRATONVM_REQUIRE_E2E` — the same escalation `require_binary`
/// applies to a missing binary, and the one CI sets.
fn warn_if_stale(binary: &std::path::Path) {
    if std::env::var_os("CRATONVM_BIN").is_some() {
        return;
    }
    let (Some(newest_src), Ok(built)) = (
        newest_source_mtime(),
        std::fs::metadata(binary).and_then(|m| m.modified()),
    ) else {
        return;
    };
    if built >= newest_src {
        return;
    }
    let msg = format!(
        "the `cratonvm` binary at {} is OLDER than the newest workspace source. \
         This test drives that binary, so it is reporting on a build that does \
         not contain the code under test — a fixed defect can read as open, and \
         an open one as fixed. Rebuild with `cargo build --release -p \
         cratonvm-cli`, or point `CRATONVM_BIN` at the binary you meant.",
        binary.display()
    );
    if require_e2e() {
        panic!("{msg}");
    }
    eprintln!("[common] WARNING: {msg}");
}

/// The [`require_binary`] contract for a **checked-in Java fixture**.
///
/// # Why this is a third helper and not a third caller of `require_binary`
///
/// A missing `cratonvm` binary or a missing JDK is an absent *toolchain* — a
/// contributor can legitimately have neither, so the default has to be a skip.
/// A missing `.java` fixture is a different animal: it is a file this
/// repository is supposed to CARRY. Its absence is a broken checkout, not a
/// broken workstation, and it is the single largest source of vacuous greens
/// in this suite.
///
/// The 2026-08-07 audit found 23 distinct `apps/<probe>/` fixtures referenced
/// by `vm/tests/*.rs` that are absent from the tree. `apps/` is `.gitignore`d
/// (line 12), so every fixture ever written there was untracked and vanished
/// for everyone but its author. The tests that drive them all had the same
/// shape:
///
/// ```ignore
/// if !probe_dir.exists() { return; }        // cargo prints `ok` in 0.00s
/// ```
///
/// # What this changes, and what it deliberately does not
///
/// It does **not** promote a missing fixture to an unconditional panic. Doing
/// that in one lane would turn ~60 quiet tests red at once on every developer
/// machine, and the fixtures cannot be reconstructed from the assertions
/// alone. What it does:
///
/// * the skip becomes **LOUD** — an `eprintln!` naming every path that was
///   searched, so `cargo test -- --nocapture` shows the fixture is gone
///   instead of showing nothing at all;
/// * [`REQUIRE_VAR`] promotes it to a panic, exactly as for a binary or a JDK,
///   so CI can assert that a green run was a real one.
///
/// Returns the first candidate that exists, so a call site can use it as its
/// lookup:
///
/// ```ignore
/// let Some(src) = common::require_fixture(
///     "wave4_a",
///     "the AtomicProbe fixture",
///     &[probe_dir().join("AtomicProbe.java")],
/// ) else { return; };
/// ```
///
/// `what` should name the fixture in the words the test's own diagnostics use;
/// it is quoted verbatim in both the skip note and the panic.
pub fn require_fixture(tag: &str, what: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    for c in candidates {
        if c.exists() {
            return Some(c.clone());
        }
    }
    let searched = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n  ");
    if require_e2e() {
        panic!(
            "[{tag}] {REQUIRE_VAR} is set, but {what} is MISSING, so this test would have skipped \
             and still reported `ok`. Searched:\n  {searched}\n\nThis is a file the repository is \
             supposed to carry, not an absent toolchain. Note `apps/` is gitignored (.gitignore \
             line 12): a fixture placed there is untracked and disappears for every other \
             checkout. `probes/` is the tracked home. Unset {REQUIRE_VAR} to go back to skipping."
        );
    }
    eprintln!(
        "[{tag}] SKIPPING: {what} is MISSING. Searched:\n  {searched}\nThis test asserts NOTHING \
         in this state. Set {REQUIRE_VAR}=1 to turn it into a failure. `apps/` is gitignored, so \
         a fixture written there is untracked — `probes/` is the tracked home."
    );
    None
}

/// The [`require_binary`] contract for a JDK home.
pub fn require_jdk(found: Option<PathBuf>) -> Option<PathBuf> {
    if found.is_none() && require_e2e() {
        panic!(
            "{REQUIRE_VAR} is set, but no usable JDK was found, so this test would have skipped \
             and still reported `ok`. Point `CRATONVM_TEST_JDK` or `JAVA_HOME` at a JDK install. \
             Unset {REQUIRE_VAR} to go back to skipping."
        );
    }
    found
}

/// Wait for `child` up to `cap`, **draining its pipes the whole time**, and
/// report what it produced either way.
///
/// # Why a test cannot just poll `try_wait`
///
/// The shape this replaces is everywhere in `vm/tests`:
///
/// ```ignore
/// let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
/// loop {
///     match child.try_wait()? {
///         Some(_) => break,
///         None if start.elapsed() < CAP => sleep(50ms),
///         None => { child.kill(); panic!("timed out") }
///     }
/// }
/// let output = child.wait_with_output()?;   // <- the first read of the pipes
/// ```
///
/// Nothing reads either pipe until after the child has exited. A Linux pipe
/// holds 64 KiB; once the child has written that much it blocks in `write` and
/// can never exit, so the parent polls out its whole timeout and reports a
/// hang. The child is not hung — it is waiting for the parent, which is
/// waiting for it.
///
/// Measured 2026-09-04 on `class_loader_unload_regression`: the probe's stdout
/// was 124 bytes and its **stderr was 101,808** (`[GC]` lines, one pair per
/// `System.gc()`, and that probe calls it ~180 times). Run with its output to a
/// file it finishes in under 20 s and prints `ok=true`; run under that poll
/// loop it "times out" at 180 s, in both jit and nojit modes, deterministically,
/// on a quiet host. It had nothing to do with class unloading.
///
/// The GC noise itself is now gated (`zgc.rs`'s logging block), which removes
/// this instance. **That is not the fix, and this is** — a test must not depend
/// on the process it drives staying under 64 KiB, and the next diagnostic
/// anyone adds should not be able to hang the suite.
///
/// # What it returns
///
/// Always the output that was captured, plus whether the cap was hit. A timeout
/// that can show what the child managed to say is a diagnosis; the poll loop's
/// bare `panic!("timed out")` is what made this cost a session.
pub struct TimedOutput {
    pub output: Output,
    pub timed_out: bool,
}

pub fn wait_draining(mut child: Child, cap: Duration) -> TimedOutput {
    // Take the pipes BEFORE the wait and read them on their own threads, so
    // neither can fill while this function is sleeping.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().expect("poll child") {
            Some(s) => break s,
            None if start.elapsed() < cap => std::thread::sleep(Duration::from_millis(50)),
            None => {
                // Killing closes the pipes, which is what lets both readers
                // reach EOF and join below.
                let _ = child.kill();
                timed_out = true;
                break child.wait().expect("reap killed child");
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    TimedOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        timed_out,
    }
}

// ---------------------------------------------------------------------------
// Waiting on a probe that is SLOW without waiting on one that is STUCK
// ---------------------------------------------------------------------------

/// What forward progress looks like in a probe's own output.
///
/// # Why a wall-clock cap could not tell those two apart
///
/// Every long-running probe in this suite is guarded by one constant: "fail if
/// the child has not exited in N seconds". The constants are livelock guards —
/// `VTHREAD_PROBE_CAP` says so in as many words — but a deadline cannot express
/// "stopped making progress", only "took too long", and the two come apart
/// badly here:
///
/// * **Profile.** Every one of those constants was derived from a `--release`
///   measurement, and `ci.yml` runs `cargo test --workspace`, which is debug.
///   Measured 2026-09-11 on one 8-core host: `VthreadGcStress` 16-23 s release
///   against 400 s+ debug, `VthreadProbe` 6.3 s release against 30-125 s
///   debug, `LoaderUnloadProbe` 15.7 s release against 400 s+ debug.
/// * **Load.** The same host at load 40 turned a 52 s run into a 125 s one
///   with no source change.
///
/// Both make a *healthy* run fail, which is the one thing a livelock guard
/// must never do — and the repair everyone reaches for, raising the constant,
/// is exactly the change that hides the hang it was there to catch.
///
/// # What this measures instead
///
/// The probe prints a counter that only ever goes DOWN, and the guard fails
/// when that counter stops moving — not when the clock runs out. A run on a
/// host so loaded it takes ten times as long still makes progress every
/// second; the 2026-09-05 hang this suite exists for made none at all, from
/// any thread, forever.
pub struct Progress<'a> {
    /// Substring introducing a counter that DECREASES as the probe advances,
    /// e.g. `"remaining="`. A line carrying a value below the lowest seen so
    /// far is forward progress and resets the stall clock; anything else —
    /// including a repeat of the same value — is not, so a probe that keeps
    /// printing while wedged is still caught.
    pub countdown_key: &'a str,
    /// Fail when no forward progress has been made for this long.
    ///
    /// This is the number that has to be defended, and it is a *starvation*
    /// budget, not a runtime one: how long a working probe can go without
    /// advancing its counter once on a host that is busy with other things.
    pub stall: Duration,
    /// Fail regardless once the whole run has taken this long.
    ///
    /// Backstop for the case the stall clock cannot see — a probe that keeps
    /// advancing but will not finish, which is a defect of a different shape.
    /// Generous on purpose; the stall clock is the guard.
    pub ceiling: Duration,
}

/// Why [`wait_watching`] stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The child exited on its own. The only outcome that is not a failure.
    Exited,
    /// No forward progress for [`Progress::stall`]; the child was killed.
    Stalled,
    /// [`Progress::ceiling`] elapsed while the child was still advancing.
    Ceiling,
}

/// Everything [`wait_watching`] observed, including on a failure.
pub struct WatchedOutput {
    pub output: Output,
    pub stop: Stop,
    /// Lowest countdown value seen; `None` if the probe never printed one,
    /// which is itself a diagnosis — the child never got as far as its first
    /// heartbeat.
    pub lowest: Option<u64>,
    /// How many times the countdown moved down.
    pub advances: usize,
    /// Wall time from spawn to stop.
    pub elapsed: Duration,
    /// Wall time since the last time the countdown moved down.
    pub since_progress: Duration,
}

impl WatchedOutput {
    /// A ready-made panic body naming what was seen and what it means, for a
    /// caller that has decided `stop != Stop::Exited` is a failure.
    pub fn diagnosis(&self, what: &str) -> String {
        let stdout = String::from_utf8_lossy(&self.output.stdout);
        let stderr = String::from_utf8_lossy(&self.output.stderr);
        let verdict = match self.stop {
            Stop::Exited => "exited",
            Stop::Stalled => {
                "STOPPED MAKING PROGRESS. This is the shape a livelock has: the \
                 process is still there, and its counter is not moving. A slow \
                 host does not do this — it advances the counter late, not \
                 never. Do NOT repair this by raising the stall budget without \
                 first establishing that the counter was moving."
            }
            Stop::Ceiling => {
                "hit the absolute ceiling while STILL ADVANCING. That is not a \
                 hang: the probe was working and did not finish. Either the \
                 workload outgrew the ceiling or something is making \
                 arbitrarily slow forward progress."
            }
        };
        format!(
            "[{what}] {verdict}\nelapsed={:.1}s  since last advance={:.1}s  \
             advances={}  lowest countdown={}\nstdout:\n{}\nstderr (tail):\n{}",
            self.elapsed.as_secs_f64(),
            self.since_progress.as_secs_f64(),
            self.advances,
            match self.lowest {
                Some(v) => v.to_string(),
                None => "never printed one".to_string(),
            },
            stdout,
            tail_chars(&stderr, 4000),
        )
    }
}

/// Last `n` characters of `s`, on a char boundary, prefixed when truncated.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let skipped = count - n;
    let body: String = s.chars().skip(skipped).collect();
    format!("...[{skipped} earlier chars elided]...\n{body}")
}

/// The digits immediately after `key` in `line`, if any.
fn countdown_in(line: &[u8], key: &[u8]) -> Option<u64> {
    if key.is_empty() {
        return None;
    }
    let at = line.windows(key.len()).position(|w| w == key)? + key.len();
    let digits: Vec<u8> = line[at..]
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// Wait for `child`, draining both pipes, and stop as soon as it exits, stops
/// making progress, or exceeds the ceiling — see [`Progress`].
///
/// Drains exactly as [`wait_draining`] does and for the same reason: nothing
/// here may depend on the child staying under a pipe buffer. It reads stdout a
/// line at a time rather than to EOF so the countdown can be watched live; one
/// `VthreadProbe` run measured 4.2 MB of stderr against 759 bytes of stdout,
/// so "the child cannot outrun us" is not theoretical.
pub fn wait_watching(mut child: Child, progress: Progress<'_>) -> WatchedOutput {
    use std::io::BufRead;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let key: Vec<u8> = progress.countdown_key.as_bytes().to_vec();
    let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
    let lowest = Arc::new(Mutex::new(None::<u64>));
    let advances = Arc::new(AtomicUsize::new(0));
    let last_advance = Arc::new(Mutex::new(Instant::now()));

    let out_pipe = child.stdout.take();
    let out_reader = {
        let collected = Arc::clone(&collected);
        let lowest = Arc::clone(&lowest);
        let advances = Arc::clone(&advances);
        let last_advance = Arc::clone(&last_advance);
        std::thread::spawn(move || {
            let Some(pipe) = out_pipe else { return };
            let mut reader = std::io::BufReader::new(pipe);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                collected
                    .lock()
                    .expect("stdout buffer")
                    .extend_from_slice(&line);
                if let Some(v) = countdown_in(&line, &key) {
                    let mut low = lowest.lock().expect("countdown");
                    if low.is_none_or(|seen| v < seen) {
                        *low = Some(v);
                        advances.fetch_add(1, Ordering::Relaxed);
                        *last_advance.lock().expect("advance clock") = Instant::now();
                    }
                }
            }
        })
    };
    let mut err_pipe = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut exited = None;
    let stop = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            exited = Some(status);
            break Stop::Exited;
        }
        if last_advance.lock().expect("advance clock").elapsed() >= progress.stall {
            break Stop::Stalled;
        }
        if start.elapsed() >= progress.ceiling {
            break Stop::Ceiling;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = match exited {
        Some(s) => s,
        None => {
            // Killing closes the pipes, which is what lets both readers reach
            // EOF and join below.
            let _ = child.kill();
            child.wait().expect("reap killed child")
        }
    };
    let elapsed = start.elapsed();
    let _ = out_reader.join();
    let stderr = err_reader.join().unwrap_or_default();
    let stdout = std::mem::take(&mut *collected.lock().expect("stdout buffer"));
    let since_progress = last_advance.lock().expect("advance clock").elapsed();
    let lowest = *lowest.lock().expect("countdown");
    let advances = advances.load(Ordering::Relaxed);
    WatchedOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        stop,
        lowest,
        advances,
        elapsed,
        since_progress,
    }
}
