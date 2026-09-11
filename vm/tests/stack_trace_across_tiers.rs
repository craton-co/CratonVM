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
//! | `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` | the refusal census counts the frames that took it, and the row moves |
//! | `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` | the callees the map supplied stop contributing frames |
//! | `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` | `main`'s line moves back to the back-edge |
//!
//! Those double as the anti-vacuity guard: a switch that reverts proves the
//! feature was actually engaged in the default arm rather than accidentally
//! always-on, which a green default arm alone can never show.
//!
//! The middle row carries a condition the other two do not, and it is not a
//! softening. Whether the hot throw passes through an artifact with inlined
//! callees is an INLINING decision, which this probe influences but does not
//! choose; `ir-splice-getstatic` going default-ON on 2026-09-09 changed it
//! here, by enlarging every body on this chain until the inline budget refused
//! a splice that used to fit. `mid` and `outer` became ordinary interpreter
//! frames — the trace stayed correct and still matches the interpreter oracle,
//! but there was no longer an inlined callee for the switch to take away.
//! Demanding that they disappear would pin that inlining decision rather than
//! the map, and go red on the next budget change as well. So the assertion
//! takes the strong form when the chain actually contributed a frame, and
//! otherwise pins that the switch still reverts *something* and reports the
//! unexercised half as an environment outcome — loud on stderr, and a failure
//! under `CRATONVM_REQUIRE_E2E`, exactly like a run where `main` never OSRed.
//!
//! `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE` (defect 4) is deliberately NOT asserted:
//! its revert shape — a compiled frame emitted beside its interpreter frame —
//! was only ever observed together with `CRATONVM_JIT_NO_INLINE=1`, and a check
//! whose expected output nobody has measured is a false red waiting to happen.
//!
//! # Flakiness, and the claim that was wrong about it
//!
//! The interesting row exists only if `main` really OSR-compiles AND the
//! artifact is entered before the loop it was compiled for ends. Until
//! 2026-09-11 this section said the second half came for free:
//!
//! > background compilation is off by default (`CRATONVM_BG_COMPILE` is
//! > opt-in), so the compile happens on the mutator at the threshold rather
//! > than racing the end of the loop from a worker thread
//!
//! **That has been false since wire-tiered-manager Step 7.**
//! `vm/src/runtime/env_cache.rs::bg_compile` returns `true` when the variable is
//! unset -- the flag is an opt-OUT, `CRATONVM_BG_COMPILE=0`, which its own
//! comment offers to "suites that still need the historical inline tier-up".
//! So the compile does race the end of the loop, and every kill-switch
//! assertion in this file was written as though it could not.
//!
//! What that cost, MEASURED on one debug binary with nothing else changing:
//!
//! * `CRATONVM_DBG_JITC=1` prints the same compile events on every run in a
//!   different ORDER. When `mid`'s optimizing body supersedes its baseline one
//!   (`c2-supersede published ... epoch_bumped=true`) relative to the third
//!   throw decides whether `probe`'s bound direct call to `outer` is still bound
//!   when that throw happens.
//! * So `after_main_osr` is produced sometimes by two JIT entries (`main`'s OSR
//!   artifact and a compiled `leaf`) and sometimes by one, with the chain
//!   interpreted -- and on a host at load 100, twice in ten runs, by NONE: the
//!   OSR body published after the loop had ended.
//! * The default arm cannot see any of that, because every one of its lines is
//!   correct either way. That is the point of the fix this file guards, and it
//!   is also why the variance went unnoticed until the reverted arms were asked
//!   to be deterministic.
//! * `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` accordingly printed three
//!   different hot rows across sixteen runs, all three of them the switch
//!   working, and the assertion demanded the shape of one of them.
//!
//! Tier-up itself was never the risk the old text worried about: the probe runs
//! 400_000 back-edges against a default `CRATONVM_TIER_OSR_THRESHOLD` of 10_000,
//! a 40x margin no host load moves. It is the PUBLICATION that raced. So the
//! kill-switch arms now run against a control that sets `CRATONVM_BG_COMPILE=0`
//! -- see the comment where that control is built -- which puts the compile back
//! on the mutator at the threshold and makes all four rows bit-identical run to
//! run, census included. The default arm keeps the shipping configuration, and
//! a run in which `main` never tiered up at all is still reported as its own
//! clearly-worded ENVIRONMENT outcome rather than dressed up as an assertion
//! failure, loud on stderr and a failure under `CRATONVM_REQUIRE_E2E`.
//!
//! # What the kill-switch arms are pinned on
//!
//! Every arm establishes that the feature ENGAGED in its own run before
//! asserting that switching it off reverted anything, and each pins the revert
//! where it is decided rather than at the shape it happens to take in a trace:
//!
//! * defect 1 on the refusal census `CRATONVM_DBG_JIT_METHOD_STATS=1` prints
//!   ([`frame_line_census`]) -- `switched-off > 0` and `answered == 0` -- plus
//!   the requirement that the row moved and that every cell that moved is
//!   attributable to the switch;
//! * defect 2 on what the map owes and no more: a callee it supplied is gone,
//!   only a callee it could have supplied may go, and nothing it did not supply
//!   moves. WHICH callees the chain carried is an inlining decision this file
//!   does not pin;
//! * defect 3 on `main` having been a COMPILED frame at the throw
//!   ([`main_was_a_compiled_frame`]), which is stronger than an OSR-compile line
//!   in the log and is exactly the reading an unmoved line would otherwise have.
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

use std::collections::BTreeMap;
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
/// `CRATONVM_DBG_JITC=1` and `CRATONVM_DBG_SWCHAIN=1` are set on every arm,
/// including the interpreter one, so the arms differ by exactly the switch under
/// test. Neither arms anything on a path the JIT runs hot -- `dbg_swchain_enabled`
/// is read inside the stack walk itself -- which is why they can be blanket
/// defaults here and `CRATONVM_DBG_JIT_METHOD_STATS` cannot; see
/// [`frame_line_census`].
fn run_arm(
    bin: &Path,
    jdk: &Path,
    classes: &Path,
    arm: &str,
    extra_env: &[(&str, &str)],
) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        .env("CRATONVM_DBG_JITC", "1")
        .env("CRATONVM_DBG_SWCHAIN", "1");
    for &(key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("-c")
        .arg(classes)
        .arg("StackTraceAfterOsr")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("[{TAG}] {arm}: could not spawn cratonvm: {e}"));
    // Generous: an arm that never tiers up runs the whole probe interpreted,
    // and a loaded host makes that slower still. A timeout here is an
    // environment report, not a verdict on the trace.
    //
    // DRAINED, and that is load-bearing rather than tidy. Every arm here sets
    // `CRATONVM_DBG_JITC=1`, a pipe holds tens of KiB, and the shape this
    // replaced -- poll `try_wait`, then `wait_with_output` -- reads neither pipe
    // until the child has exited. A child that writes more than the buffer
    // blocks in `write` and can never exit, so the parent polls out its whole
    // cap and reports a hang that is not one. It had not bitten this file only
    // because this probe's JIT log is currently small enough, which is a
    // property of the diagnostics someone else owns. See `wait_draining`.
    let timed = common::wait_draining(child, Duration::from_secs(600));
    let out = timed.output;
    assert!(
        !timed.timed_out,
        "[{TAG}] {arm}: the probe did not finish within the cap. What it managed to \
         say:\nstdout:\n{}\nstderr tail:\n{}",
        String::from_utf8_lossy(&out.stdout),
        tail(&String::from_utf8_lossy(&out.stderr))
    );
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

/// The marker `jit::tiered::dump_method_stats_to_stderr` prints the refusal
/// census behind, under `CRATONVM_DBG_JIT_METHOD_STATS=1`.
const CENSUS_MARK: &str = "compiled-frame lines: ";

/// Why each compiled frame in a run's stack traces did, or did not, get a line
/// -- `jit::compiled_frame_line_counts`, parsed by slot name.
///
/// # Why defect 1's arm is pinned on this and not on the shape of a trace
///
/// `switched-off` counts the frames that took the kill switch's arm, at the
/// site that decides it, and the three [`answered`] slots count the frames the
/// recovery resolved. A switch that has stopped being read reads as
/// `switched-off == 0` whatever else the run did; a switch read by some arms of
/// `activation_bci` and not others reads as both counters non-zero. Neither
/// statement can be made from a trace, which prints `(Unknown Source)` for four
/// different refusals plus the switch.
///
/// The assertion that stood in its place was `any(|f| f.line <= 0)` on the hot
/// row, and it failed about one run in ten. MEASURED, 2026-09-11: under
/// `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` one debug binary printed
/// `leaf:-1 ... main:62` fourteen times, `leaf:-1 ... main:66` once and
/// `leaf:25 ... main:62` once. All three are the switch working -- on the third
/// the only compiled frame in the row was `main`'s OSR artifact, whose revert
/// shows up as a LINE moving back to the back-edge. The kill-switch control
/// (`CRATONVM_BG_COMPILE=0`, see the comment where it is built) is what removed
/// that variance at its source; this census is what makes the arm's claim true
/// by construction rather than by luck.
///
/// # Why `CRATONVM_DBG_JIT_METHOD_STATS` is on the control and not every arm
///
/// It is not a print-only flag: it also arms `getfield_census_counting_enabled`
/// (`vm/src/jit/helpers.rs`) and `code_ptr_memo_census_enabled`
/// (`jit/src/lib.rs`), which count on paths the JIT runs hot. Setting it on the
/// default arm would make that arm's timing differ from the shipping
/// configuration it is there to measure. On the control, where tier-up is
/// counter-driven on the mutator, the extra counting cannot change what gets
/// compiled -- and it is on both sides of every pair, so no A/B is disturbed.
fn frame_line_census(stderr: &str, arm: &str) -> BTreeMap<String, u64> {
    let Some(line) = stderr.lines().find(|l| l.contains(CENSUS_MARK)) else {
        panic!(
            "[{TAG}] {arm}: the probe printed no `{CENSUS_MARK}` line, so \
             CRATONVM_DBG_JIT_METHOD_STATS=1 did not reach the VM or the exit dump stopped \
             printing it. Defect 1's arm is pinned on that census, so this is an environment \
             failure, not a verdict on the trace.\nstderr tail:\n{}",
            tail(stderr)
        );
    };
    let body = line
        .split(CENSUS_MARK)
        .nth(1)
        .unwrap_or("")
        .split('|')
        .next()
        .unwrap_or("");
    let mut out = BTreeMap::new();
    for token in body.split_whitespace() {
        if let Some((name, value)) = token.rsplit_once('=') {
            if let Ok(n) = value.parse::<u64>() {
                out.insert(name.to_string(), n);
            }
        }
    }
    assert!(
        out.contains_key("switched-off") && out.contains_key("single-pass"),
        "[{TAG}] {arm}: the `{CENSUS_MARK}` line no longer carries the slot names this file \
         reads. `jit::FRAME_LINE_SLOT_NAMES` is the one source of them and this parser has \
         fallen behind it.\ngot: {line}"
    );
    out
}

/// The three census slots that mean a compiled frame's bci WAS recovered, as
/// opposed to refused for one of the four reasons -- or the kill switch --
/// beside them.
fn answered(census: &BTreeMap<String, u64>) -> u64 {
    ["single-pass", "ir", "npe-trap"]
        .iter()
        .filter_map(|k| census.get(*k))
        .sum()
}

/// One census slot, or zero.
fn slot(census: &BTreeMap<String, u64>, name: &str) -> u64 {
    census.get(name).copied().unwrap_or_default()
}

/// Was `main` a COMPILED frame at a throw in this run?
///
/// [`osr_entered_main`] answers a different and weaker question: whether an OSR
/// compile of `main` ever happened. It is true in a run where the artifact was
/// published after the hot loop had already ended, so `probe()` was called from
/// an interpreted `main` and there was no OSR-entered frame in the trace at all.
/// MEASURED: that is what two runs in ten looked like on a host at load 100 with
/// background compilation on, and it is the reading defect 3's arm has to be
/// able to exclude before it can call an unmoved `main` line a regression.
///
/// `CRATONVM_DBG_SWCHAIN=1` prints the compiled-frame chain that
/// `active_compiled_frames` walked, and that walk happens only when a trace is
/// captured -- three times in this probe, none of them before `main` could have
/// OSR-compiled. So a `boundary=StackTraceAfterOsr.main` line can only have come
/// from a throw taken while `main` was executing its own compiled artifact,
/// which is the precondition the OSR arm needs and the only one it needs.
///
/// Unlike `CRATONVM_DBG_JIT_METHOD_STATS`, this switch arms nothing on a hot
/// path: `dbg_swchain_enabled` is read inside the stack walk itself, so it is
/// safe to set on every arm and keep them comparable.
fn main_was_a_compiled_frame(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.contains("boundary=StackTraceAfterOsr.main"))
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
    //
    // Every arm below is the SAME control configuration plus ONE switch, and it
    // is compared against that control rather than against the default arm.
    // Three things about the control, each of them a repair:
    //
    // `CRATONVM_BG_COMPILE=0`. The header of this file used to say background
    // compilation was off by default, "so the compile happens on the mutator at
    // the threshold rather than racing the end of the loop from a worker
    // thread". That has been FALSE since wire-tiered-manager Step 7:
    // `vm/src/runtime/env_cache.rs::bg_compile` returns `true` when the variable
    // is unset, and its own comment offers `CRATONVM_BG_COMPILE=0` to "suites
    // that still need the historical inline tier-up". The consequence is the
    // whole flakiness this section carried. MEASURED, 2026-09-11, sixteen runs
    // of the hot row on one debug binary: `CRATONVM_DBG_JITC=1` prints the same
    // compile events every time in a different ORDER, and which frames of
    // `after_main_osr` are COMPILED moves with it -- sometimes `main`'s OSR
    // artifact and a compiled `leaf`, sometimes `main`'s alone, and on a host at
    // load 100, twice in ten runs, not even `main`: the OSR body published after
    // the loop it was for had already ended. Every assertion here was written as
    // if that set were fixed. With the flag at 0 the compile happens on the
    // mutator at the back-edge threshold, and all four rows below are
    // bit-identical run to run, census included.
    //
    // `CRATONVM_JIT_IR_SPLICE_GETSTATIC=0`. Restores the inlined callee the
    // map arm needs to take away. `ir-splice-getstatic` landing default-ON on
    // 2026-09-09 is what removed it: `leaf` reads the static `table`, so
    // admitting `getstatic` for splicing made every body on this chain bigger
    // (`outer`'s optimizing body went 493 -> 731 bytes) and the inline budget
    // then refused a splice that used to fit. The previous version of that arm
    // ran in the shipping configuration, found nothing to take away, and
    // reported a coverage gap -- five runs in five, which is how it took the
    // `CRATONVM_REQUIRE_E2E` leg of CI red with it.
    //
    // `CRATONVM_DBG_JIT_METHOD_STATS=1`. Prints the refusal census
    // [`frame_line_census`] reads, which is where the bci arm's revert is
    // pinned. It also arms hot-path counting (`getfield_census_counting_enabled`,
    // `code_ptr_memo_census_enabled`), which is why it is on the CONTROL and not
    // a blanket default -- under `CRATONVM_BG_COMPILE=0` the tier-up is
    // counter-driven, so the extra counting can no longer change what gets
    // compiled.
    //
    // None of the three is under test. Each is held equal across the pair.
    const CONTROL_ENV: &[(&str, &str)] = &[
        ("CRATONVM_BG_COMPILE", "0"),
        ("CRATONVM_DBG_JIT_METHOD_STATS", "1"),
        ("CRATONVM_JIT_IR_SPLICE_GETSTATIC", "0"),
    ];
    const CONTROL_ARM: &str = "BG_COMPILE=0 DBG_JIT_METHOD_STATS=1 JIT_IR_SPLICE_GETSTATIC=0";
    let arm_env = |switch: &'static str| -> Vec<(&'static str, &'static str)> {
        let mut env = CONTROL_ENV.to_vec();
        env.push((switch, "1"));
        env
    };

    let (ctl_out, ctl_err) = run_arm(&bin, &jdk, &classes, CONTROL_ARM, CONTROL_ENV);
    let ctl = parse_row(&ctl_out, &ctl_err, HOT_ROW, CONTROL_ARM);
    let ctl_census = frame_line_census(&ctl_err, CONTROL_ARM);

    // The control must produce the trace the DEFAULT arm produced. That is a
    // cross-configuration claim and not an A/B: it says that who compiled a
    // frame (mutator or worker), and whether a callee was spliced, do not move
    // a line -- an inlined callee is supplied by the frame map with its own
    // recorded bci and a callee that is not supplies its own interpreter frame.
    // If this ever fails, the map disagrees with the interpreter about one
    // throw, which is this file's own defect one configuration over.
    assert_eq!(
        ctl,
        jit_rows[2],
        "[{TAG}] the kill-switch control ({CONTROL_ARM}) prints a different {HOT_ROW} from the \
         default arm. Neither of those three flags is under test and none of them may move a \
         line.\ncontrol: {}\ndefault arm: {}",
        render(&ctl),
        render(&jit_rows[2])
    );
    // Engagement, at the control, once for all three arms: the hot throw was
    // taken from a COMPILED `main` frame. Under `CRATONVM_BG_COMPILE=0` the OSR
    // compile happens on the mutator at the back-edge threshold, 40x before the
    // loop ends, so this is deterministic -- the race that made it not so is the
    // first paragraph above.
    assert!(
        main_was_a_compiled_frame(&ctl_err),
        "[{TAG}] no stack walk in the control crossed a compiled `main` frame, so the hot throw \
         was taken from an INTERPRETED `main` even with the tier-up on the mutator. The \
         OSR-compile line the guard above found says the compile happened; something between \
         that and the frame refused the entry (grep the stderr for `OSR-reject`, `OSR-DENY`, \
         `osr optimizing`). Every arm below would then compare two interpreted \
         rows.\ncontrol: {}\nstderr tail:\n{}",
        render(&ctl),
        tail(&ctl_err)
    );
    // The first assertion in this file that a compiled frame's bci was
    // RECOVERED at all. Every assertion above is satisfied by a trace whose
    // every line came from an interpreter frame's own LineNumberTable, and
    // would go on being satisfied if the recovery were deleted.
    assert!(
        answered(&ctl_census) > 0,
        "[{TAG}] the control resolved NO compiled frame's bci in the whole run (single-pass + \
         ir + npe-trap = 0), so this file measured nothing about the recovery it exists to \
         guard and the arms below have nothing to revert.\ncensus: {ctl_census:?}"
    );
    assert_eq!(
        slot(&ctl_census, "switched-off"),
        0,
        "[{TAG}] the control reports frames that took a kill switch's arm, so this environment \
         already has CRATONVM_JIT_NO_COMPILED_FRAME_LINES or CRATONVM_JIT_NO_IR_FRAME_LINES \
         set. Every A/B below then compares that configuration against itself and proves \
         nothing.\ncensus: {ctl_census:?}"
    );

    // ---- defect 1's switch: a compiled frame goes back to carrying no bci ---
    //
    // Pinned WHERE IT IS DECIDED -- the refusal census -- and then checked to be
    // observable in the row. The assertion that stood here was
    // `no_lines.iter().any(|f| f.line <= 0)`, and before the control above it
    // failed about one run in ten, because on those runs the only compiled frame
    // in the row was `main`'s OSR artifact and its revert shows up as a LINE
    // moving back to the back-edge, not as a non-positive number anywhere.
    const NO_LINES_ARM: &str = "CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1";
    let (no_lines_out, no_lines_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        NO_LINES_ARM,
        &arm_env("CRATONVM_JIT_NO_COMPILED_FRAME_LINES"),
    );
    let no_lines = parse_row(&no_lines_out, &no_lines_err, HOT_ROW, NO_LINES_ARM);
    let no_lines_census = frame_line_census(&no_lines_err, NO_LINES_ARM);
    assert!(
        slot(&no_lines_census, "switched-off") > 0,
        "[{TAG}] {NO_LINES_ARM} left `switched-off` at zero, so not one compiled frame in the \
         whole run took the kill switch's arm. The switch has stopped being read by \
         `compiled_frame_bci_enabled` (vm/src/jit/conservative_roots.rs) -- and with it goes \
         the only way to attribute a suspect line in a warmed-up trace to bci recovery rather \
         than to the LineNumberTable, inside one binary.\ncensus: {no_lines_census:?}\ncontrol \
         census: {ctl_census:?}"
    );
    assert_eq!(
        answered(&no_lines_census),
        0,
        "[{TAG}] {NO_LINES_ARM} still RESOLVED {} compiled frame bci(s), so the switch is read \
         by some arms of `activation_bci` and not others. A half-reverted switch is worse than \
         either state: the trace it produces is neither the historical answer nor the current \
         one.\ncensus: {no_lines_census:?}",
        answered(&no_lines_census)
    );
    // Defect 1's switch alone: it takes the BCI away, not the FRAME. Losing a
    // frame here would mean it has picked up defect 2's half.
    assert_eq!(
        methods(&no_lines),
        methods(&ctl),
        "[{TAG}] {NO_LINES_ARM} changed the FRAMES of {HOT_ROW}, not just their \
         lines.\ngot: {}\ncontrol: {}",
        render(&no_lines),
        render(&ctl)
    );
    // ... and the revert is OBSERVABLE, in one of exactly two shapes. A frame
    // whose own bci is gone reports the historical `-1`; `main` reports the
    // back-edge it tiered up at, because `stackwalker`'s display-only override
    // is the bci of a compiled half that no longer has one. Any OTHER cell
    // moving is this switch reaching past the recovery it names.
    let moved: Vec<(&str, i32, i32)> = no_lines
        .iter()
        .zip(ctl.iter())
        .filter(|(got, want)| got.line != want.line)
        .map(|(got, want)| (got.method.as_str(), want.line, got.line))
        .collect();
    assert!(
        !moved.is_empty(),
        "[{TAG}] {NO_LINES_ARM} counted {} frame(s) that took its arm and yet {HOT_ROW} is \
         byte-identical to the control. The refusal is being counted and then not applied, so \
         the census and the trace disagree about one run.\ngot: {}\ncontrol: {}",
        slot(&no_lines_census, "switched-off"),
        render(&no_lines),
        render(&ctl)
    );
    for (method, was, now) in &moved {
        assert!(
            *now <= 0 || *method == "main",
            "[{TAG}] {NO_LINES_ARM} moved `{method}` from line {was} to line {now} in \
             {HOT_ROW}. This switch has two legal shapes -- a compiled frame reporting the \
             historical `-1`, and `main` falling back to the back-edge it tiered up at -- and a \
             positive line on any other frame is neither. It means the reverted path is \
             inventing a line rather than refusing to supply one.\ngot: {}\ncontrol: {}",
            render(&no_lines),
            render(&ctl)
        );
    }

    // ---- defect 2's switch: the inlined callees stop contributing frames ----
    //
    // WHICH of `leaf`, `mid` and `outer` the chain carries is an inlining
    // decision this probe influences but does not choose, and the version of
    // this arm that demanded `mid` AND `outer` was pinning that decision rather
    // than the map. What the map owes is narrower and is what is asserted: a
    // callee it supplied is gone when it is switched off, only a callee it could
    // have supplied may go, and nothing it did not supply moves. MEASURED,
    // 2026-09-11, five runs: the control's five frames become four, with `mid`
    // gone, every time.
    const NO_MAP_ARM: &str = "CRATONVM_JIT_NO_INLINE_FRAME_MAP=1";
    let (no_map_out, no_map_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        NO_MAP_ARM,
        &arm_env("CRATONVM_JIT_NO_INLINE_FRAME_MAP"),
    );
    let no_map = parse_row(&no_map_out, &no_map_err, HOT_ROW, NO_MAP_ARM);
    let survivors = methods(&no_map);
    let dropped: Vec<&str> = methods(&ctl)
        .into_iter()
        .filter(|m| !survivors.contains(m))
        .collect();
    if dropped.is_empty() {
        // The chain contributed nothing even here, so the only thing left to
        // pin is that the switch is still wired at all: it reverts the
        // innermost frame's line too, because `apply_npe_trap_site` gates that
        // on the same `inline_frame_chains_enabled()`.
        assert_ne!(
            no_map,
            ctl,
            "[{TAG}] {NO_MAP_ARM} changed NOTHING about {HOT_ROW}: same frames, same lines. \
             Every other reading of this row is accounted for, so this one means the switch has \
             stopped being read -- check that both halves still name it \
             (jit/src/x64/inlining.rs and vm/src/jit/conservative_roots.rs).\ngot: {}",
            render(&no_map)
        );
        let note = format!(
            "[{TAG}] NOTE: the hot throw did not pass through an artifact with inlined callees, \
             even with CRATONVM_JIT_IR_SPLICE_GETSTATIC=0 holding the inlining decision where \
             this probe was built for it, so the inline-frame map's CHAIN half went unexercised \
             in this run. The switch is pinned to still revert something -- the assertion just \
             above -- and the trace still matches the interpreter oracle, so this is a coverage \
             gap rather than a defect. But this was the last lever this file had for that half, \
             so the gap is now total. Check `CRATONVM_DBG_JITC=1` for `nest-static leaf(I)I ... \
             -> SPLICED` on this chain: if a budget change refused the splice again, this arm \
             needs a new lever, not a softer assertion.\ncontrol: {}\nno-map arm: {}",
            render(&ctl),
            render(&no_map)
        );
        assert!(
            !common::require_e2e(),
            "{note}\n\nCRATONVM_REQUIRE_E2E is set, so this incomplete run is a failure."
        );
        eprintln!("{note}");
    } else {
        // `probe` carries an exception table, so the planner refuses to inline
        // it (`inline-resolve REFUSED ... callee-exception-table`), and `main`
        // is the OSR root. Neither can ever be a spliced callee, so neither may
        // disappear when the map does.
        for kept in ["probe", "main"] {
            assert!(
                survivors.contains(&kept),
                "[{TAG}] {NO_MAP_ARM} dropped `{kept}` from {HOT_ROW}. That frame is not a \
                 spliced callee -- `probe` carries an exception table and is refused by the \
                 inline planner, `main` is the OSR root -- so the inline-frame map cannot be \
                 what was supplying it. Switching the map off has taken away a frame that came \
                 from somewhere else.\ngot: {}\ncontrol: {}",
                render(&no_map),
                render(&ctl)
            );
        }
        for gone in &dropped {
            assert!(
                ["leaf", "mid", "outer"].contains(gone),
                "[{TAG}] {NO_MAP_ARM} dropped `{gone}` from {HOT_ROW}, which is not on the \
                 chain this probe splices (`outer` -> `mid` -> `leaf`). The switch is reverting \
                 more than the inline-frame map.\ngot: {}\ncontrol: {}",
                render(&no_map),
                render(&ctl)
            );
        }
        // Every frame that SURVIVED keeps the line it had. The map's chain half
        // supplies frames; it must not move the ones it did not supply.
        for name in &survivors {
            if *name == "leaf" {
                // The one documented exception: `apply_npe_trap_site` gates the
                // innermost frame's line on the same switch, so `leaf` reports
                // the historical `-1` here for the same reason defect 1's arm
                // produces one.
                continue;
            }
            let (a, b) = (line_of(&no_map, name), line_of(&ctl, name));
            assert_eq!(
                a.map(|f| f.line),
                b.map(|f| f.line),
                "[{TAG}] {NO_MAP_ARM} moved `{name}` in {HOT_ROW}. The map's chain half \
                 supplies FRAMES; a frame it did not supply must keep its own \
                 line.\ngot: {}\ncontrol: {}",
                render(&no_map),
                render(&ctl)
            );
        }
    }

    // ---- defect 3's switch: the OSR frame reports its back-edge again ------
    const NO_REFRESH_ARM: &str = "CRATONVM_JIT_NO_OSR_PC_REFRESH=1";
    let (no_refresh_out, no_refresh_err) = run_arm(
        &bin,
        &jdk,
        &classes,
        NO_REFRESH_ARM,
        &arm_env("CRATONVM_JIT_NO_OSR_PC_REFRESH"),
    );
    let no_refresh = parse_row(&no_refresh_out, &no_refresh_err, HOT_ROW, NO_REFRESH_ARM);
    assert_ne!(
        line_of(&no_refresh, "main").map(|f| f.line),
        line_of(&ctl, "main").map(|f| f.line),
        "[{TAG}] {NO_REFRESH_ARM} leaves `main` on the same line as the control in {HOT_ROW}, \
         so it no longer reverts the OSR continuation registry. The control asserted that this \
         configuration takes the hot throw from a COMPILED `main` frame, so the reading that \
         there was no OSR frame to revert is excluded. Without the revert a wrong line in a \
         warmed-up trace can no longer be attributed to the OSR pc refresh inside one \
         binary.\ngot: {}\ncontrol: {}",
        render(&no_refresh),
        render(&ctl)
    );
    // It reverts THAT half and only that half: the frames themselves stay.
    assert_eq!(
        methods(&no_refresh),
        EXPECTED_METHODS.to_vec(),
        "[{TAG}] {NO_REFRESH_ARM} changed the FRAMES of {HOT_ROW}, not just `main`'s line. It \
         is defect 3's switch alone; losing frames here means it has picked up defect 2's half \
         as well.\ngot: {}",
        render(&no_refresh)
    );
}
