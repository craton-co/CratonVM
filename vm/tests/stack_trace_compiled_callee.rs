// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An exception raised BY compiled code must not lose the frames that raised it.
//!
//! Sibling of `stack_trace_across_tiers.rs`, and it covers the half that one
//! deliberately cannot: there the throw is an implicit NPE whose *callees were
//! spliced*, so every frame the trace needs is still described by one artifact.
//! Here the callee is padded past `MAX_INLINE_BYTECODE_SIZE` (325) so it is
//! compiled and **called**, and its activation is a real compiled frame of its
//! own — the one an implicit NPE unwinds before the throwable is built.
//!
//! Drives the checked-in probe `probes/StackTraceCompiledCallee.java`.
//!
//! # The defect this pins
//!
//! An implicit NPE in compiled code is not thrown where it happens. The null
//! check is an inline `TEST`/`JZ` to a stub that flags the NPE, loads the
//! `i64::MIN` deopt sentinel and runs the **epilogue**; the
//! `java/lang/NullPointerException` is constructed afterwards, from the
//! interpreter. `fillInStackTrace` therefore runs on a stack the compiled
//! frames have already left:
//!
//! ```text
//!              HotSpot 25                   CratonVM before
//! cold         [big:42 probe:56 main:65]    same
//! after_warm   [big:42 probe:56 main:67]    [probe:56 main:67]
//! after_osr    [big:42 probe:56 main:71]    [main:71]
//! ```
//!
//! Three separate things had to be true for that to be fixed, and this file
//! asserts all three because each failed independently:
//!
//!  1. the frames are **snapshotted** inside the helper, while they are still
//!     on the stack (`jit::helpers::snapshot_trap_frames`);
//!  2. the snapshot reaches **every** door that constructs the NPE. The first
//!     cut reached one. `materialize_implicit_signal` — the constructor for an
//!     implicit signal routed into a compiled callee's OWN handler, which is
//!     exactly what `probe()`'s `catch` is — attached nothing, and two restash
//!     paths replaced a live snapshot with a fresh, shallower one. That is the
//!     `after_osr` row;
//!  3. the recovered frame carries a **line**. An inline null check is not a
//!     GC-capable call, so it publishes no safepoint id and `activation_bci`
//!     correctly refused the stale one — `big:-1`. The emitter now records the
//!     trapping bci per site (`CompiledMethod::npe_trap_map`) and a ten-byte
//!     cold trampoline carries the site id to the helper.
//!
//! # Shape, not a golden string
//!
//! Nothing here compares against a literal line number, for the same reason as
//! the sibling file: a test that must be edited whenever its own fixture is
//! edited gets deleted rather than updated.
//!
//! * **The interpreter is the oracle.** `CRATONVM_DISABLE_JIT=1` on the SAME
//!   binary and the SAME `.class` file must produce a byte-identical
//!   `(method, line)` sequence for all three rows.
//! * the frame count and the sequence `big, probe, main` in all three rows;
//! * `big` and `probe` report the same line in all three rows — one throw
//!   site, so warm-up cannot move them;
//! * no frame reports a non-positive line.
//!
//! # Why this is not a `regression-suite` vector
//!
//! Because the oracle would have to be HotSpot, and HotSpot cannot be one for
//! this shape without a `-XX:` flag the suite does not pass:
//! `OmitStackTraceInFastThrow` is on by default and replaces the trace of a
//! repeated implicit exception with a shared, frameless one. The same trap
//! already cost `RJitStackTraceLines` a draft. Using `CRATONVM_DISABLE_JIT=1`
//! on one binary sidesteps it entirely and cannot go stale on a probe edit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::io::Read;
use std::time::Duration;

mod common;

const TAG: &str = "stack_trace_compiled_callee";

/// The probe's three throw reports, in the order it prints them.
const ROWS: [&str; 3] = ["cold", "after_warm", "after_osr"];

/// The three frames every row must have, innermost first.
const EXPECTED_METHODS: [&str; 3] = ["big", "probe", "main"];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    method: String,
    line: i32,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn probe_source() -> Option<PathBuf> {
    common::require_fixture(
        TAG,
        "the StackTraceCompiledCallee probe (probes/StackTraceCompiledCallee.java)",
        &[workspace_root()
            .join("probes")
            .join("StackTraceCompiledCallee.java")],
    )
}

fn probe_classes_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join("stack-trace-across-tiers")
        .join(TAG)
}

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

/// `-g` is not optional: without a `LineNumberTable` every assertion about a
/// line becomes vacuous, and the interpreter arm would agree with the compiled
/// arm on `-1` everywhere.
fn compile_probe(javac: &Path, src: &Path) -> Option<PathBuf> {
    let dir = probe_classes_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::remove_file(dir.join("StackTraceCompiledCallee.class"));
    let out = match Command::new(javac)
        .args(["-g", "--release", "21", "-d"])
        .arg(&dir)
        .arg(src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[{TAG}] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_text = String::from_utf8_lossy(&out.stderr);
        if stderr_text.contains("release version") && stderr_text.contains("not supported") {
            eprintln!(
                "[{TAG}] javac cannot target --release 21 ({}); skipping.",
                stderr_text.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("StackTraceCompiledCallee.class").exists(),
        "[{TAG}] the probe failed to compile — fix probes/StackTraceCompiledCallee.java. javac \
         stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// Run one arm. Every arm is the SAME binary and the SAME classes; only
/// `extra_env` differs, which is what makes this an A/B rather than a
/// comparison of two builds.
fn run_arm(
    bin: &Path,
    jdk: &Path,
    classes: &Path,
    arm: &str,
    extra_env: &[(&str, &str)],
) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home").arg(jdk).env("CRATONVM_DBG_JITC", "1");
    for &(key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("-c")
        .arg(classes)
        .arg("StackTraceCompiledCallee")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("[{TAG}] {arm}: could not spawn cratonvm: {e}"));

    // DRAIN BOTH PIPES WHILE WAITING, on their own threads.
    //
    // This used to `try_wait()` in a sleep loop and call `wait_with_output()`
    // only after the child had exited — which cannot work when the child
    // outproduces one OS pipe buffer. `CRATONVM_DBG_JITC=1` (set unconditionally
    // above, because the compile lines are half of what the arms are compared
    // on) writes ~55 KiB to stderr against a 64 KiB buffer on Linux: 9 KiB of
    // headroom, or about one bg-compile line per 90. A run that emits a few
    // more — a loaded host recompiling, or the `NO_NPE_TRAP_LINES` arm taking a
    // different path — fills the buffer, the child blocks in `write`, and the
    // loop below waits the full 600 s for an exit that can never come. The
    // failure reads as "probe timed out", which points at the VM; the VM
    // finishes this probe in three seconds when its stderr goes anywhere else.
    //
    // Reading concurrently is the only fix that keeps the timeout meaningful:
    // it is now a bound on the probe, not on the probe's chattiness.
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let timeout = Duration::from_secs(600);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[{TAG}] {arm}: probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[{TAG}] {arm}: wait failed: {e}"),
        }
    }
    // Both readers see EOF when the child's ends close, which the exit above
    // has already caused; joining cannot outlive it.
    let stdout = stdout_reader
        .join()
        .unwrap_or_else(|_| panic!("[{TAG}] {arm}: stdout reader panicked"));
    let stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| panic!("[{TAG}] {arm}: stderr reader panicked"));
    (
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

/// Parse one `<row>=len=N [method:line ...]` report into frames.
fn parse_row(stdout: &str, stderr: &str, row: &str, arm: &str) -> Vec<Frame> {
    let prefix = format!("{row}=");
    let Some(text) = stdout.lines().find(|l| l.starts_with(&prefix)) else {
        panic!(
            "[{TAG}] {arm}: the probe printed no `{prefix}` line, so it did not reach the throw \
             this test is about.\nstdout:\n{stdout}\nstderr tail:\n{}",
            tail(stderr)
        );
    };
    let open = text.find('[').unwrap_or_else(|| {
        panic!("[{TAG}] {arm}: no `[` in the {row} report — the probe's format changed: {text}")
    });
    let close = text.rfind(']').unwrap_or_else(|| {
        panic!("[{TAG}] {arm}: no `]` in the {row} report — the probe's format changed: {text}")
    });
    let frames: Vec<Frame> = text[open + 1..close]
        .split_whitespace()
        .map(|token| {
            let (qualified, number) = token.rsplit_once(':').unwrap_or_else(|| {
                panic!("[{TAG}] {arm}: frame token {token:?} has no `:line` in {row}: {text}")
            });
            let method = match qualified.rsplit_once('.') {
                Some((_, m)) => m,
                None => qualified,
            };
            let line: i32 = number.parse().unwrap_or_else(|e| {
                panic!("[{TAG}] {arm}: frame token {token:?} has an unparsable line in {row}: {e}")
            });
            Frame {
                method: method.to_string(),
                line,
            }
        })
        .collect();
    let declared: usize = text
        .split_once("len=")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(usize::MAX);
    assert_eq!(
        declared,
        frames.len(),
        "[{TAG}] {arm}: parsed {} frame(s) from the {row} report but the probe declared len={}. \
         The parser in this test is wrong, not the VM: {text}",
        frames.len(),
        declared
    );
    frames
}

fn render(frames: &[Frame]) -> String {
    let mut s = format!("len={} [", frames.len());
    for f in frames {
        s.push_str(&f.method);
        s.push(':');
        s.push_str(&f.line.to_string());
        s.push(' ');
    }
    s.push(']');
    s
}

fn methods(frames: &[Frame]) -> Vec<&str> {
    frames.iter().map(|f| f.method.as_str()).collect()
}

fn line_of<'a>(frames: &'a [Frame], method: &str) -> Option<&'a Frame> {
    frames.iter().find(|f| f.method == method)
}

fn tail(text: &str) -> String {
    const KEEP: usize = 4000;
    if text.len() <= KEEP {
        return text.to_string();
    }
    let mut cut = text.len() - KEEP;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("… (truncated)\n{}", &text[cut..])
}

/// Did this run compile `big` at all? Without that, every row came from the
/// interpreter and the assertions proved only that the interpreter agrees with
/// itself — an ENVIRONMENT outcome, not a verdict.
fn compiled_big(stderr: &str) -> bool {
    stderr.lines().any(|l| {
        l.contains("StackTraceCompiledCallee.big") && !l.contains("FAILED") && l.contains("compile")
    })
}

#[test]
fn an_exception_raised_by_compiled_code_keeps_the_frame_that_raised_it() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[{TAG}] cratonvm binary not found; build it with `cargo build --release -p \
             cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[{TAG}] no usable JDK found (set CRATONVM_TEST_JDK or JAVA_HOME). skipping.");
        return;
    };
    let Some(src) = probe_source() else {
        return;
    };
    let javac = jdk.join(if cfg!(windows) {
        "bin/javac.exe"
    } else {
        "bin/javac"
    });
    let Some(classes) = compile_probe(&javac, &src) else {
        return;
    };

    let (jit_out, jit_err) = run_arm(&bin, &jdk, &classes, "default", &[]);
    let (interp_out, interp_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        "CRATONVM_DISABLE_JIT=1",
        &[("CRATONVM_DISABLE_JIT", "1")],
    );

    let jit_rows: Vec<Vec<Frame>> = ROWS
        .iter()
        .map(|r| parse_row(&jit_out, &jit_err, *r, "default"))
        .collect();
    let interp_rows: Vec<Vec<Frame>> = ROWS
        .iter()
        .map(|r| parse_row(&interp_out, &interp_err, *r, "CRATONVM_DISABLE_JIT=1"))
        .collect();

    for (i, row) in ROWS.iter().enumerate() {
        let frames = &jit_rows[i];

        assert_eq!(
            methods(frames),
            EXPECTED_METHODS.to_vec(),
            "[{TAG}] {row}: wrong frames. Losing `big` is the whole defect — an implicit NPE is \
             constructed after the compiled activation that raised it has already run its \
             epilogue, so the trace names none of the code that threw.\ngot:  {}\n\
             interpreter reference: {}",
            render(frames),
            render(&interp_rows[i])
        );

        for f in frames {
            assert!(
                f.line > 0,
                "[{TAG}] {row}: frame `{}` reports line {}, which is not a source line. `-1` on \
                 `big` is the trapping frame's own bci going missing: an inline null check is not \
                 a GC-capable call, so it publishes no safepoint id and the only evidence is the \
                 trap site the emitter recorded (`CompiledMethod::npe_trap_map`).\ngot: {}",
                f.method,
                f.line,
                render(frames)
            );
        }

        assert_eq!(
            frames,
            &interp_rows[i],
            "[{TAG}] {row}: the JIT arm and the CRATONVM_DISABLE_JIT=1 arm disagree. Warm-up \
             changed the trace, which is the whole defect this test exists for.\n\
             jit         : {}\ninterpreter : {}",
            render(frames),
            render(&interp_rows[i])
        );
    }

    // ONE THROW SITE. `big` and `probe` are the same two source lines in all
    // three rows; only `main`'s call site legitimately moves.
    for method in ["big", "probe"] {
        let seen: Vec<i32> = jit_rows
            .iter()
            .map(|frames| {
                line_of(frames, method)
                    .unwrap_or_else(|| panic!("[{TAG}] no `{method}` frame in {}", render(frames)))
                    .line
            })
            .collect();
        assert!(
            seen.iter().all(|l| *l == seen[0]),
            "[{TAG}] `{method}` reports a different line depending on warm-up: {:?} across rows \
             {:?}. It is ONE throw site — only `main`'s line may move.",
            seen,
            ROWS
        );
    }

    // ---- anti-vacuity: did anything actually run compiled? ----------------
    if !compiled_big(&jit_err) {
        let note = format!(
            "[{TAG}] DID NOT COMPILE `big`: no `[cratonvm-jitc]` compile line named \
             StackTraceCompiledCallee.big, so every row was produced by the interpreter and the \
             assertions above proved only that the interpreter agrees with itself. This is an \
             ENVIRONMENT outcome, not a verdict on the fix. The kill-switch arms below are \
             SKIPPED because they cannot revert something that never engaged.\nstderr tail:\n{}",
            tail(&jit_err)
        );
        assert!(
            !common::require_e2e(),
            "{note}\n\nCRATONVM_REQUIRE_E2E is set, so this incomplete run is a failure."
        );
        eprintln!("{note}");
        return;
    }

    // ---- the kill switches must still revert ------------------------------
    // Each is BOTH a pin on the switch and the proof that the default arm's
    // green came from the feature rather than from the feature never having
    // been needed.

    // The snapshot itself: without it the raising frame is gone from at least
    // one hot row, which is the historical answer.
    let (no_snap_out, no_snap_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        "CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1",
        &[("CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT", "1")],
    );
    let no_snap: Vec<Vec<Frame>> = ROWS
        .iter()
        .map(|r| {
            parse_row(
                &no_snap_out,
                &no_snap_err,
                *r,
                "CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1",
            )
        })
        .collect();
    assert!(
        no_snap.iter().any(|frames| frames.len() < 3),
        "[{TAG}] CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1 still produces a full trace in every row, \
         so it no longer reverts the snapshot — and with it goes the only way to attribute a \
         missing frame in a warmed-up trace to this recovery inside one binary.\n\
         rows: {}\ndefault: {}",
        no_snap.iter().map(|f| render(f)).collect::<Vec<_>>().join(" | "),
        jit_rows.iter().map(|f| render(f)).collect::<Vec<_>>().join(" | ")
    );

    // The trap-site table: the recovered frame goes back to carrying no bci.
    let (no_trap_out, no_trap_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        "CRATONVM_JIT_NO_NPE_TRAP_LINES=1",
        &[("CRATONVM_JIT_NO_NPE_TRAP_LINES", "1")],
    );
    let no_trap: Vec<Vec<Frame>> = ROWS
        .iter()
        .map(|r| parse_row(&no_trap_out, &no_trap_err, *r, "CRATONVM_JIT_NO_NPE_TRAP_LINES=1"))
        .collect();
    assert!(
        no_trap
            .iter()
            .any(|frames| line_of(frames, "big").is_some_and(|f| f.line <= 0)),
        "[{TAG}] CRATONVM_JIT_NO_NPE_TRAP_LINES=1 leaves `big` with a real line in every row, so \
         it no longer reverts the trap-site table. Either the switch stopped being read by both \
         halves (the emitter's ten-byte trampoline and the walk's override read the same name \
         deliberately), or `big` was never a snapshotted frame in this run and the default arm's \
         line proves nothing about the table.\nrows: {}\ndefault: {}",
        no_trap.iter().map(|f| render(f)).collect::<Vec<_>>().join(" | "),
        jit_rows.iter().map(|f| render(f)).collect::<Vec<_>>().join(" | ")
    );
}
