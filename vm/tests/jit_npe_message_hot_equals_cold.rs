// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compiling a method must not take JEP 358's message off its
//! `NullPointerException`.
//!
//! Drives the checked-in probe `probes/L2JitNpeProbe.java`, which asks nine
//! null-dereference shapes TWICE in one process: once cold, then again after
//! 200 000 warm-up calls with a live receiver. Each shape is its own static
//! method, so the compiler treats them independently, and no JDK class is
//! involved anywhere.
//!
//! # The defect this pins
//!
//! ```text
//!                       HotSpot                       CratonVM (2026-09-10)
//! readField cold        …because "h" is null          …because "h" is null
//! readField hot         …because "h" is null          null
//! readArrayLen hot      …because "h" is null          null
//! invokeOn hot          Cannot invoke "…get()"…       null
//! invokeOnString hot    Cannot read field "s"…        null
//! writeField hot        Cannot assign field "value"…  null
//! caughtHere hot        Cannot read field "value"…    null
//! ```
//!
//! The last row's HotSpot column is `java -XX:-OmitStackTraceInFastThrow`, and
//! that is not a softening of the oracle — it is the one row where default
//! HotSpot answers a different QUESTION. `OmitStackTraceInFastThrow` is on by
//! default and lets C2 replace an implicit exception it catches in its own
//! compiled body with a shared, preallocated, message-less one, so default
//! `java` prints `null` there too. CratonVM has no such optimisation, so the
//! message is the answer it must give, and the flagged arm is the one that asks
//! about the message rather than about the optimisation.
//! `probes/StackTraceAfterOsr.java` documents the same flag for the same
//! reason.
//!
//! **Cold, this VM produced JEP 358's message; hot, it produced none at all** —
//! `getMessage()` was null, so a caller saw a bare
//! `java.lang.NullPointerException`. The array shapes had a third answer, worse
//! than it looks because it is plausible: a HotSpot-verbatim ACTION half
//! (`Cannot read the array length`) with the `because "…" is null` clause
//! silently missing.
//!
//! It is a diagnostic-quality defect and not a correctness one — the exception
//! type, the site and the control flow were always right. What made it worth a
//! test is WHEN it happens: a null dereference in a hot loop is the case a
//! developer is most likely to be debugging and least likely to reproduce cold.
//!
//! See `docs/internal/fixed-bugs/the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md`.
//!
//! # The oracle is the VM's own cold row, not a golden string
//!
//! The two rows of a pair differ in NOTHING but whether the method has been
//! compiled, so `hot == cold` is the whole assertion and it cannot go stale:
//! both sides are measured in the same run, by the same binary, on the same
//! `.class` file. A golden string would have to be re-verified against a JDK
//! every time the probe moved a field, and would rot into a list nobody
//! re-derives.
//!
//! The cold row is a real oracle and not a tautology: it is produced by the
//! INTERPRETER's message builder (`runtime/interpreter/opcodes.rs` and
//! `invoke.rs`), which has its own HotSpot-verbatim tests in
//! `runtime/exceptions.rs::helpful_npe_tests`, and which was already correct
//! when the hot row was empty. The two are different code paths that must
//! agree, which is exactly the property the defect broke.
//!
//! # Two ways this file could pass while asserting nothing, both closed
//!
//! * **The probe never compiling.** Then every "hot" row is a second
//!   interpreted row and `hot == cold` holds trivially. The run is required to
//!   report at least one `full-compile` of a probe method under
//!   `CRATONVM_DBG_JITC=1`, and the absence is an ENVIRONMENT outcome — loud on
//!   stderr, and a failure under `CRATONVM_REQUIRE_E2E`.
//! * **The messages being empty on BOTH sides.** `-XX:-ShowCodeDetails…` and
//!   `CRATONVM_HELPFUL_NPE_OPCODES=0` both make every row `null`, which also
//!   satisfies `hot == cold`. So the cold rows are additionally required to
//!   carry a `because "…" is null` clause, which is the half the gates remove.
//!
//! # Flags
//!
//! None. The point is the DEFAULT configuration: this defect was invisible
//! precisely because nothing had to be switched on to meet it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

/// Tag on every diagnostic this file emits.
const TAG: &str = "jit_npe_message_hot_equals_cold";

/// The nine shapes the probe prints, in the order it prints them. Each yields
/// two rows, `"<shape> cold"` and `"<shape> hot"`.
const SHAPES: [&str; 9] = [
    "readField",
    "readArrayLen",
    "invokeOn",
    "invokeOnString",
    "writeField",
    // The sixth reaches a DIFFERENT constructor: it catches the NPE in the
    // compiled method's own handler, where the throwable is built by
    // `jit::helpers::materialize_implicit_signal` rather than by the
    // interpreter's post-JIT drain. Two doors, one defect, and only this row
    // can tell them apart.
    "caughtHere",
    // The three ARRAY shapes. They are the half the filed page did not ask
    // about, and the only ones that had a HotSpot-verbatim ACTION half with the
    // `because "…" is null` clause silently missing -- a message that reads
    // exactly like one HotSpot prints when it cannot name the expression.
    "lenOf",
    "loadOf",
    "storeTo",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

/// The checked-in probe. Only the tracked `probes/` home is searched, for the
/// reason `stack_trace_across_tiers.rs` gives for the same choice: `apps/` is
/// gitignored, a fixture there is one a fresh checkout does not have, and a
/// path into it would also make this file owe a row to
/// `probe_fixture_census.rs`.
fn probe_source() -> Option<PathBuf> {
    common::require_fixture(
        TAG,
        "the L2JitNpeProbe probe (probes/L2JitNpeProbe.java)",
        &[workspace_root().join("probes").join("L2JitNpeProbe.java")],
    )
}

/// Per-test-binary class output, so no other harness can race our `javac`.
fn probe_classes_dir() -> PathBuf {
    workspace_root().join("target").join(TAG)
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
    let target = workspace_root().join("target");
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

/// `-g` so the `because "h" is null` clause names the PARAMETER rather than
/// `<local0>`. Both spellings are HotSpot-legal and the assertion here is
/// `hot == cold` either way, but a run with debug info is the one a developer
/// actually meets, and it is the one whose clause is worth pinning.
fn compile_probe(javac: &Path, src: &Path) -> Option<PathBuf> {
    let dir = probe_classes_dir();
    let _ = std::fs::create_dir_all(&dir);
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("L2JitNpeProbe.class"));
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
        out.status.success() && dir.join("L2JitNpeProbe.class").exists(),
        "[{TAG}] the probe failed to compile — fix probes/L2JitNpeProbe.java. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// Run the probe once. `CRATONVM_DBG_JITC=1` is on so the anti-vacuity check
/// below can see whether anything actually compiled; it changes no message.
///
/// Drained through `common::wait_draining`, and that is not a style choice.
/// `CRATONVM_DBG_JITC=1` on this probe writes hundreds of KiB to stderr, a pipe
/// holds tens of KiB, and the poll-`try_wait`-then-`wait_with_output` shape
/// reads neither pipe until the child has exited — so the child blocks in
/// `write`, the parent polls out its whole cap, and the run is reported as a
/// hang. Measured here on 2026-09-11: this probe finishes in **0.8 s** by hand
/// and "timed out after 900 s" under that shape. See `wait_draining`'s own doc,
/// which is about exactly this and cost someone else a session first.
fn run_probe(bin: &Path, jdk: &Path, classes: &Path) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        .env("CRATONVM_DBG_JITC", "1")
        .arg("-c")
        .arg(classes)
        .arg("L2JitNpeProbe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("[{TAG}] could not spawn cratonvm: {e}"));
    // Generous against the real time: the probe drives 9 x 200 000 warm-up
    // calls, and a run that never compiles does all of them interpreted. A
    // timeout here is an environment report, not a verdict on any message.
    let timed = common::wait_draining(child, Duration::from_secs(600));
    let stdout = String::from_utf8_lossy(&timed.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&timed.output.stderr).into_owned();
    assert!(
        !timed.timed_out,
        "[{TAG}] the probe did not finish within the cap. What it managed to say:\nstdout:\n{stdout}\nstderr tail:\n{}",
        tail(&stderr)
    );
    (stdout, stderr)
}

/// The probe prints one row per line as `<label> |<text>|`. The delimiters are
/// the probe's own and are what make an EMPTY message distinguishable from a
/// missing row.
fn row(stdout: &str, label: &str) -> String {
    let prefix = format!("{label} |");
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return rest
                .strip_suffix('|')
                .unwrap_or_else(|| panic!("[{TAG}] row `{label}` is not terminated by `|`: {line}"))
                .to_string();
        }
    }
    panic!(
        "[{TAG}] the probe printed no `{label}` row. Its output was:\n{stdout}\n\nThis file \
         parses the probe's own format; if the probe changed, change the parser with it."
    );
}

/// Did this run compile anything at all? Without that, every "hot" row is a
/// second interpreted row and `hot == cold` proves nothing.
fn compiled_something(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.contains("full-compile L2JitNpeProbe."))
}

#[test]
fn a_compiled_null_dereference_keeps_the_message_its_interpreted_twin_has() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[{TAG}] no cratonvm binary; skipping");
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[{TAG}] no JDK; skipping");
        return;
    };
    let Some(src) = probe_source() else { return };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let Some(classes) = compile_probe(&javac, &src) else {
        return;
    };

    let (stdout, stderr) = run_probe(&bin, &jdk, &classes);
    assert!(
        stdout.contains("DONE L2JitNpeProbe"),
        "[{TAG}] the probe did not run to completion.\nstdout:\n{stdout}\nstderr tail:\n{}",
        tail(&stderr)
    );

    // ---- the assertion ----------------------------------------------------
    for shape in SHAPES {
        let cold = row(&stdout, &format!("{shape} cold"));
        let hot = row(&stdout, &format!("{shape} hot"));
        assert_eq!(
            hot, cold,
            "[{TAG}] `{shape}`: warming the method CHANGED its NullPointerException. The two \
             rows differ in nothing but whether the method has been compiled, so this is the \
             compiler and cannot be anything else.\n  cold: {cold}\n  hot:  {hot}\n\nThe machinery \
             is `runtime/interpreter/jit_npe_message.rs`, which rebuilds the JEP 358 message from \
             the trapping method's own bytecode, and the trap SITE the emitter records for it \
             (`x64::inlining::record_npe_trap_site`, and its optimizing-tier twin \
             `ir_lower::record_npe_trap_site`). A row that regresses to a bare \
             `NullPointerException: null` means the site was not described; one that keeps the \
             action half and loses the `because` clause means the site was described but its bci \
             was refused."
        );
    }

    // ---- anti-vacuity 1: the cold rows must be MESSAGED --------------------
    //
    // `-XX:-ShowCodeDetailsInExceptionMessages` and
    // `CRATONVM_HELPFUL_NPE_OPCODES=0` both make every row `null`, which
    // satisfies `hot == cold` while asserting nothing about either path.
    for shape in SHAPES {
        let cold = row(&stdout, &format!("{shape} cold"));
        assert!(
            cold.contains("because \"") && cold.contains("\" is null"),
            "[{TAG}] `{shape} cold` carries no `because \"…\" is null` clause: {cold}\n\nThat is \
             the half both gates above remove, so without it the `hot == cold` assertion is \
             satisfied by two empty messages. The INTERPRETER builds this row; if it has \
             regressed, the defect is in `runtime/interpreter/opcodes.rs` / `invoke.rs`, not in \
             the JIT."
        );
    }

    // ---- anti-vacuity 2: something must actually have compiled -------------
    if !compiled_something(&stderr) {
        let note = format!(
            "[{TAG}] NOTHING COMPILED: no `[cratonvm-jitc] full-compile L2JitNpeProbe.…` line \
             appeared, so every `hot` row was produced by the interpreter and the assertions \
             above proved only that the interpreter agrees with itself. This is an ENVIRONMENT \
             outcome: the probe drives 200 000 calls per shape against a default \
             CRATONVM_JIT_THRESHOLD of 500, so if nothing compiled then either this build has \
             the JIT off or a threshold moved.\nstderr tail:\n{}",
            tail(&stderr)
        );
        assert!(
            !common::require_e2e(),
            "{note}\n\nCRATONVM_REQUIRE_E2E is set, so this incomplete run is a failure."
        );
        eprintln!("{note}");
    }
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
