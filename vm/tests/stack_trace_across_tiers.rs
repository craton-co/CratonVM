// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Warming a method up must not change the stack trace it throws.
//!
//! Drives the checked-in probe `probes/StackTraceAfterOsr.java`, which throws
//! from ONE site three times in one process. Only the amount of prior warm-up
//! differs between the three throws, so a correct VM prints the same five
//! frames every time and only `main`'s line legitimately moves — it is a
//! different call site in `main` each time.
//!
//! # The defect this pins
//!
//! Before `claude/audit-impl-20260901` the third row collapsed:
//!
//! ```text
//! after_main_osr   HotSpot : len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]
//!                  CratonVM: len=3 [leaf:25                  probe:-1 main:62]
//! ```
//!
//! Four independent defects stacked into that one line — a compiled frame with
//! no bci at all (`probe:-1`), inlined callees contributing no frame (`mid` and
//! `outer` gone), an OSR frame reporting the back-edge it tiered up at rather
//! than the call it is suspended in (`main:62` for `main:66`), and a compiled
//! frame emitted beside its own interpreter frame. See
//! jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md.
//!
//! Severity is zero for program results and high for diagnosability, which is
//! exactly why it survived so long: nothing throws, nothing logs, the trace
//! looks plausible, and it only degrades *after* warm-up — that is, only in the
//! runs anyone cares about. The fix landed with **no test anywhere under `vm/`
//! or `jit/`**. This file is that test.
//!
//! # Shape, not a golden string
//!
//! Every line number in the witness above moves the moment anyone edits the
//! probe, and a test that must be updated on every edit of its own fixture gets
//! deleted rather than updated. So nothing here compares against a literal line
//! number. What it asserts instead:
//!
//! * **The interpreter is the oracle.** `CRATONVM_DISABLE_JIT=1` on the SAME
//!   binary and the SAME `.class` file must produce a byte-identical
//!   `(method, line)` sequence for all three rows. That single comparison
//!   covers all four defects at once and can never go stale, because both sides
//!   are measured in the same run pair.
//! * the frame **count** and the **method sequence** `leaf, mid, outer, probe,
//!   main` in all three rows;
//! * `leaf`, `mid`, `outer` and `probe` report the **same** line in all three
//!   rows — they are one throw site, so warm-up cannot move them;
//! * **no frame reports a non-positive line.** `-1` was the visible symptom of
//!   defect 1 and `-2` is the JDK's "native method"; neither is legal for any
//!   frame in this probe;
//! * `main`'s line, which is the one value that legitimately differs per row,
//!   is checked against the line the probe's own source puts that row's
//!   `System.out.println` on — derived from the fixture, so editing the probe
//!   moves both sides together. This is what pins defect 3 independently of the
//!   interpreter arm.
//!
//! # The kill switches are pinned too
//!
//! The four switches are what make this A/B-able inside one binary, and a
//! switch that quietly stops reverting is a silent loss of exactly the
//! diagnostic that would find the next regression. Three are asserted to still
//! revert their own half:
//!
//! | switch | expected effect on `after_main_osr` |
//! |---|---|
//! | `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` | a non-positive line reappears |
//! | `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` | `mid` and `outer` disappear |
//! | `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` | `main`'s line moves back to the back-edge |
//!
//! Those double as the anti-vacuity guard: a switch that reverts proves the
//! feature was actually engaged in the default arm rather than accidentally
//! always-on, which a green default arm alone can never show.
//!
//! `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE` (defect 4) is deliberately NOT asserted:
//! its revert shape — a compiled frame emitted beside its interpreter frame —
//! was only ever observed together with `CRATONVM_JIT_NO_INLINE=1`, and a check
//! whose expected output nobody has measured is a false red waiting to happen.
//!
//! # Flakiness
//!
//! The interesting row exists only if `main` really OSR-compiles, so a test
//! that assumed it would is a test that fails for the wrong reason on a busy
//! machine. Two things make that risk small and one makes it visible:
//!
//! * tier-up here is **counter-driven, not time-driven** — the probe runs
//!   400_000 back-edges against a default `CRATONVM_TIER_OSR_THRESHOLD` of
//!   10_000, a 40x margin that no amount of host load moves;
//! * background compilation is **off by default** (`CRATONVM_BG_COMPILE` is
//!   opt-in), so the compile happens on the mutator at the threshold rather
//!   than racing the end of the loop from a worker thread. The arms below
//!   deliberately do not force it either way — an A/B is only an A/B when one
//!   variable differs;
//! * and if `main` nevertheless did not tier up, that is reported as its OWN
//!   clearly-worded outcome — witnessed by the `[cratonvm-jitc] OSR-compile`
//!   line under `CRATONVM_DBG_JITC=1`, not inferred from the trace — instead of
//!   being dressed up as an assertion failure. It is loud on stderr and
//!   `CRATONVM_REQUIRE_E2E=1` turns it into a failure, so CI still cannot go
//!   green on a run that measured nothing.
//!
//! # Flags
//!
//! Every switch here is passed to a **child process** through
//! `Command::env`. None of them is `set_var`'d into this process: a declared
//! `CRATONVM_*` flag is served from a snapshot latched on the first read of any
//! flag, so mutating `environ` after another test in this binary has touched
//! `flags()` changes nothing the VM will read (see `types/src/flags.rs` and
//! `types/tests/flag_env_mutation_guard.rs`). The override hooks
//! (`with_thread_overrides`) cannot reach a separate process at all, which
//! settles the choice: these flags are read by the `cratonvm` binary we spawn,
//! so the environment of that spawn is the supported way to set them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

/// Tag on every diagnostic this file emits.
const TAG: &str = "stack_trace_across_tiers";

/// The probe's three throw reports, in the order it prints them. The last one
/// is the row that degraded.
const ROWS: [&str; 3] = ["before_any_warm", "after_helper_warm", "after_main_osr"];

/// The row whose frames come from compiled and OSR-entered code.
const HOT_ROW: &str = "after_main_osr";

/// The five frames every row must have, innermost first.
const EXPECTED_METHODS: [&str; 5] = ["leaf", "mid", "outer", "probe", "main"];

/// One parsed `StackTraceElement`.
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

/// The checked-in probe. Only the tracked `probes/` home is searched on
/// purpose: the gitignored fixture tree is where 23 other fixtures went to die,
/// and a path into it would also make this file owe a row to
/// `probe_fixture_census.rs`. A MISSING fixture is reported through
/// `common::require_fixture`, which makes the skip loud and makes
/// `CRATONVM_REQUIRE_E2E=1` fail it.
fn probe_source() -> Option<PathBuf> {
    common::require_fixture(
        TAG,
        "the StackTraceAfterOsr probe (probes/StackTraceAfterOsr.java)",
        &[workspace_root()
            .join("probes")
            .join("StackTraceAfterOsr.java")],
    )
}

/// Per-test-binary class output, so no other harness can race our `javac`.
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

/// `-g` is not optional here: without a LineNumberTable every assertion about a
/// line becomes vacuous, and the interpreter arm would agree with the compiled
/// arm on `-1` everywhere.
fn compile_probe(javac: &Path, src: &Path) -> Option<PathBuf> {
    let dir = probe_classes_dir();
    let _ = std::fs::create_dir_all(&dir);
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("StackTraceAfterOsr.class"));
    let out = match Command::new(javac)
        .args(["-g", "--release", "21", "-d"])
        .arg(&dir)
        .arg(src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[{TAG}] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_text = String::from_utf8_lossy(&out.stderr);
        if stderr_text.contains("release version") && stderr_text.contains("not supported") {
            eprintln!(
                "[{TAG}] javac cannot target --release 21 ({}); skipping. Point JAVA_HOME or \
                 CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_text.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("StackTraceAfterOsr.class").exists(),
        "[{TAG}] the probe failed to compile — fix probes/StackTraceAfterOsr.java. javac \
         stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// Run one arm. Every arm is the SAME binary and the SAME classes; only
/// `extra_env` differs, which is what makes this an A/B rather than a
/// comparison of two builds.
///
/// `CRATONVM_DBG_JITC=1` is set on every arm, including the interpreter one, so
/// the arms differ by exactly the switch under test.
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
        .arg("StackTraceAfterOsr")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("[{TAG}] {arm}: could not spawn cratonvm: {e}"));
    // Generous: an arm that never tiers up runs the whole probe interpreted,
    // and a loaded host makes that slower still. A timeout here is an
    // environment report, not a verdict on the trace.
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
    let out = child
        .wait_with_output()
        .expect("collect probe output after exit");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Parse one `<row>=len=N [Class.method:line ...]` report into frames.
fn parse_row(stdout: &str, stderr: &str, row: &str, arm: &str) -> Vec<Frame> {
    let prefix = format!("{row}=");
    let Some(text) = stdout.lines().find(|l| l.starts_with(&prefix)) else {
        panic!(
            "[{TAG}] {arm}: the probe printed no `{prefix}` line, so it did not reach the throw \
             this test is about. Did the VM fail to boot?\nstdout:\n{stdout}\nstderr tail:\n{}",
            tail(stderr)
        );
    };
    let open = text.find('[').unwrap_or_else(|| {
        panic!("[{TAG}] {arm}: no `[` in the {row} report — the probe's own format changed: {text}")
    });
    let close = text.rfind(']').unwrap_or_else(|| {
        panic!("[{TAG}] {arm}: no `]` in the {row} report — the probe's own format changed: {text}")
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

    // Cross-check the probe's own `len=` against what we parsed, so a parser
    // that silently drops a frame cannot make the count assertions pass.
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

/// Render frames the way the probe does, for failure messages.
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

/// The last few KiB of a stream — full JIT debug output is far too large to
/// paste into an assertion message.
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

/// The 1-based source line of the single line of the probe containing `needle`.
///
/// This is how `main`'s expected line is derived rather than written down: the
/// probe prints each row with `System.out.println("<row>=" + probe())`, so the
/// bci `main` is suspended at resolves to exactly that source line. Editing the
/// probe moves the fixture and this expectation together.
fn unique_source_line(source: &str, needle: &str, path: &Path) -> usize {
    let hits: Vec<usize> = source
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .map(|(i, _)| i + 1)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "[{TAG}] expected exactly one line of {} to contain {needle:?}, found {}. This test \
         derives `main`'s expected line from the probe's own source instead of hard-coding it, \
         which needs each row's `System.out.println(\"<row>=\" + probe())` to stay on ONE line \
         and to appear once. Put it back on one line, or drop this derivation and rely on the \
         CRATONVM_DISABLE_JIT reference arm alone.",
        path.display(),
        hits.len()
    );
    hits[0]
}

/// Did this run OSR-enter `main`? The success line is
/// `[cratonvm-jitc] OSR-compile <class>.<method><desc> entry_pc=…`, and its
/// `reuse`/`recompile` siblings mean the same thing here (an artifact was
/// published and entered). The refusal line `OSR-compile FAILED …` shares the
/// prefix, so it has to be excluded or every run looks engaged.
fn osr_entered_main(stderr: &str) -> bool {
    stderr.lines().any(|l| {
        l.contains("StackTraceAfterOsr.main")
            && !l.contains("FAILED")
            && (l.contains("] OSR-compile ")
                || l.contains("] OSR-reuse ")
                || l.contains("] OSR-recompile "))
    })
}

#[test]
fn a_warmed_up_stack_trace_keeps_every_frame_and_every_line() {
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
    let source_text = std::fs::read_to_string(&src)
        .unwrap_or_else(|e| panic!("[{TAG}] read {}: {e}", src.display()));

    // ---- the two reference arms -------------------------------------------
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

        // SHAPE. Five frames, named, innermost first — in every row, because
        // warm-up is the only thing that differs between them.
        assert_eq!(
            methods(frames),
            EXPECTED_METHODS.to_vec(),
            "[{TAG}] {row}: wrong frames. Losing `mid`/`outer` is defect 2 — the callees the JIT \
             inlined into their caller's artifact contributing no frame at all.\ngot:  {}\n\
             interpreter reference: {}",
            render(frames),
            render(&interp_rows[i])
        );

        // NO -1. The single highest-value assertion on this page: a compiled
        // frame with no recovered bci resolves to `-1` / `(Unknown Source)`,
        // and `-2` is the JDK's "native method" — neither is legal for any
        // frame of this probe.
        for f in frames {
            assert!(
                f.line > 0,
                "[{TAG}] {row}: frame `{}` reports line {}, which is not a source line. `-1` is \
                 defect 1 — a JIT-compiled frame carrying no bytecode index at all.\ngot: {}",
                f.method,
                f.line,
                render(frames)
            );
        }

        // THE ORACLE. Same binary, same .class file, one flag apart: the
        // interpreter has no tiers to lose frames across, so any difference
        // here IS the defect, whatever new shape it takes.
        assert_eq!(
            frames,
            &interp_rows[i],
            "[{TAG}] {row}: the JIT arm and the CRATONVM_DISABLE_JIT=1 arm disagree. Warm-up \
             changed the trace, which is the whole defect this test exists for.\n\
             jit         : {}\ninterpreter : {}",
            render(frames),
            render(&interp_rows[i])
        );

        // `main` is the ONE frame whose line legitimately moves — a different
        // call site per row — so it is checked against the probe's own source
        // rather than against its neighbours.
        let want_main = unique_source_line(&source_text, &format!("\"{row}=\""), &src) as i32;
        let main_frame = line_of(frames, "main")
            .unwrap_or_else(|| panic!("[{TAG}] {row}: no `main` frame in {}", render(frames)));
        assert_eq!(
            main_frame.line, want_main,
            "[{TAG}] {row}: `main` reports line {} but the probe prints that row from line \
             {want_main} of {}. A LOW value here is defect 3 — an OSR-entered frame reporting the \
             back-edge it tiered up at instead of the call it is suspended in.\ngot: {}",
            main_frame.line,
            src.display(),
            render(frames)
        );
    }

    // ONE THROW SITE. `leaf`, `mid`, `outer` and `probe` are the same four
    // source lines in all three rows; only prior warm-up differs. If any of
    // them moves, a tier boundary invented a line.
    for method in ["leaf", "mid", "outer", "probe"] {
        let seen: Vec<i32> = jit_rows
            .iter()
            .map(|frames| {
                line_of(frames, method)
                    .unwrap_or_else(|| {
                        panic!("[{TAG}] no `{method}` frame in {}", render(frames))
                    })
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

    // ---- anti-vacuity: did the hot row actually run compiled code? ---------
    if !osr_entered_main(&jit_err) {
        let note = format!(
            "[{TAG}] DID NOT TIER UP: no `[cratonvm-jitc] OSR-compile StackTraceAfterOsr.main …` \
             line appeared, so `{HOT_ROW}` was produced by the interpreter and the assertions \
             above proved only that the interpreter agrees with itself. This is an ENVIRONMENT \
             outcome, not a verdict on the fix: the probe drives 400_000 back-edges against a \
             default CRATONVM_TIER_OSR_THRESHOLD of 10_000, so if it did not tier up, either a \
             threshold moved, the OSR door refused the method (grep the stderr for `OSR-reject`, \
             `OSR-DENY`, `OSR-compile FAILED`), or this build has the JIT off. The kill-switch \
             arms below are SKIPPED because they cannot revert something that never engaged.\n\
             stderr tail:\n{}",
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
    // Each of these is BOTH a pin on the switch and the proof that the default
    // arm's green came from the feature rather than from the feature never
    // having been needed.

    // Defect 1's switch: a compiled frame goes back to carrying no bci.
    let (no_lines_out, no_lines_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        "CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1",
        &[("CRATONVM_JIT_NO_COMPILED_FRAME_LINES", "1")],
    );
    let no_lines = parse_row(
        &no_lines_out,
        &no_lines_err,
        HOT_ROW,
        "CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1",
    );
    assert!(
        no_lines.iter().any(|f| f.line <= 0),
        "[{TAG}] CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1 no longer restores the historical `-1` on \
         any frame of {HOT_ROW}. Either the switch has stopped reverting — and with it the only \
         way to attribute a suspect line in a warmed-up trace to bci recovery rather than to the \
         LineNumberTable, inside one binary — or nothing in that row is a compiled frame any \
         more.\ngot: {}\ndefault arm: {}",
        render(&no_lines),
        render(&jit_rows[2])
    );

    // Defect 2's switch: the inlined callees stop contributing frames.
    let (no_map_out, no_map_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        "CRATONVM_JIT_NO_INLINE_FRAME_MAP=1",
        &[("CRATONVM_JIT_NO_INLINE_FRAME_MAP", "1")],
    );
    let no_map = parse_row(
        &no_map_out,
        &no_map_err,
        HOT_ROW,
        "CRATONVM_JIT_NO_INLINE_FRAME_MAP=1",
    );
    for gone in ["mid", "outer"] {
        assert!(
            line_of(&no_map, gone).is_none(),
            "[{TAG}] CRATONVM_JIT_NO_INLINE_FRAME_MAP=1 still shows `{gone}` in {HOT_ROW}, so it \
             no longer reverts the inline-frame map. Either the switch stopped being read by both \
             halves (the emitter in jit/src/x64/inlining.rs and the walk in \
             vm/src/jit/conservative_roots.rs read the same name deliberately), or `{gone}` was \
             never inlined in this run and the default arm's five frames prove nothing about the \
             map.\ngot: {}\ndefault arm: {}",
            render(&no_map),
            render(&jit_rows[2])
        );
    }

    // Defect 3's switch: the OSR frame goes back to reporting its back-edge.
    let (no_refresh_out, no_refresh_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        "CRATONVM_JIT_NO_OSR_PC_REFRESH=1",
        &[("CRATONVM_JIT_NO_OSR_PC_REFRESH", "1")],
    );
    let no_refresh = parse_row(
        &no_refresh_out,
        &no_refresh_err,
        HOT_ROW,
        "CRATONVM_JIT_NO_OSR_PC_REFRESH=1",
    );
    let reverted_main = line_of(&no_refresh, "main").map(|f| f.line);
    let default_main = line_of(&jit_rows[2], "main").map(|f| f.line);
    assert_ne!(
        reverted_main, default_main,
        "[{TAG}] CRATONVM_JIT_NO_OSR_PC_REFRESH=1 leaves `main` on the same line as the default \
         arm in {HOT_ROW}, so it no longer reverts the OSR continuation registry. Without that \
         revert a wrong line in a warmed-up trace can no longer be attributed to the OSR pc \
         refresh inside one binary.\ngot: {}\ndefault arm: {}",
        render(&no_refresh),
        render(&jit_rows[2])
    );
    // It reverts THAT half and only that half: the frames themselves stay.
    assert_eq!(
        methods(&no_refresh),
        EXPECTED_METHODS.to_vec(),
        "[{TAG}] CRATONVM_JIT_NO_OSR_PC_REFRESH=1 changed the FRAMES of {HOT_ROW}, not just \
         `main`'s line. It is defect 3's switch alone; losing frames here means it has picked up \
         defect 2's half as well.\ngot: {}",
        render(&no_refresh)
    );
}
