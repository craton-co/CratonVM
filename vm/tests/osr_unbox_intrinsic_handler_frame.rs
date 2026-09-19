// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An inline intrinsic that claims an `invokevirtual` inside a protected range
//! must still deliver that site's NPE to the method's own handler, with the
//! handler's locals live.
//!
//! `compile_osr_artifact` admits a method with a non-empty exception table on
//! one promise (RBC.6b): every throwing site inside a protected range publishes
//! a reason-9 (`PendingException`) precise frame, so
//! `route_osr_exception_out_of_artifact` may deduce that no stashed frame means
//! the throw was outside every protected range, and propagate. The predicate
//! that checks it, `first_unsupported_precise_frame_site`, reads the BYTECODE,
//! and clears opcode `0xb6` because `precise_frame_publishing_opcode` says the
//! ORDINARY invokevirtual lowering publishes.
//!
//! The BOX_UNBOX region then substitutes its own lowering for
//! `Integer.intValue` / `Long.longValue`, whose null-receiver edge is a
//! reason-6 (`ReceiverTypeChanged`) deopt stub rather than a reason-9
//! publication. That is sound, for a reason nothing else in the tree records: a
//! reason-6 deopt is not a weaker publication, it is a STRONGER action — it
//! abandons the compiled frame and hands a reconstructed one back to the
//! interpreter, which raises the NPE and searches the exception table itself.
//! The reason-9 frame is for an exception arriving INSIDE the compiled body
//! that has to be routed without leaving it.
//!
//! This pins that. If the intrinsic ever grows an edge that stays in compiled
//! code without publishing, the handler is skipped or entered on stale locals.
//! `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`
//! records the reading that was retracted to get here.
//!
//! # Why this asserts ENGAGEMENT and not only the answer
//!
//! Every property this probe depends on can be lost silently, and each loss
//! leaves it printing `PASS` while measuring nothing:
//!
//! * one `putstatic`, array load or `ldc` added inside the `try` refuses the
//!   whole method for OSR (`first_unsupported_precise_frame_site`), so no
//!   compiled body exists to be wrong;
//! * the BOX_UNBOX family is a default that has been flipped twice, and with it
//!   off the site is an ordinary CALL and the substitution under test never
//!   happens.
//!
//! So the run is made under `CRATONVM_DBG_JITC=1` and
//! `CRATONVM_DBG_ATOMIC_INTRINSIC=1`, and both witnesses are asserted: the OSR
//! compile of `arm()V`, and the intrinsic resolving `Integer.intValue()I` — a
//! site that exists nowhere in the fixture but inside that `try`.
//!
//! Both witnesses were checked against a run that should LOSE them, because an
//! engagement assertion nobody has seen fail is an engagement assertion nobody
//! has tested:
//!
//! | arm | box-unbox witness | OSR witness | probe answer |
//! |---|---|---|---|
//! | as landed | present | present | correct |
//! | `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | **gone** | present | correct |
//! | a `putstatic` planted inside the `try` | present | **gone** | correct |
//!
//! The third row is why the OSR check is spelled the way it is. Its first
//! spelling — `contains("OSR-compile") && contains("...arm()V")` — also matched
//! `OSR-compile FAILED ...arm()V ... method marked OSR-denied`, the exact
//! negative it existed to catch, and passed on that row. A substring witness
//! that matches its own negation is worse than none: it reads as coverage.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

/// The oracle line, produced by a real JDK 25 running the byte-identical
/// fixture. `caught` is `200_000 / 500`; `bad` and `drift` are the handler's
/// own reads of its pre-loop locals.
const ORACLE: &str = "caught=400 bad=0 drift=0 escaped=0 sum=6486400";

/// 200 000 iterations of a boxed add finish in about a second. Generous,
/// because a loaded CI host is exactly when it is slowest.
const TIMEOUT: Duration = Duration::from_secs(300);

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("osr_unbox_intrinsic_handler_frame_fixtures")
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

/// Compile the fixture if the checked-in `.class` is missing.
///
/// A `javac` that cannot be LAUNCHED is the one legitimate skip. A `javac` that
/// ran and rejected the source is a broken fixture and must fail loudly —
/// reporting that as "toolchain missing" is how a test becomes a permanent
/// vacuous pass.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("UnboxPreOsrLocalProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("UnboxPreOsrLocalProbe.java");
    if !source.exists() {
        return false;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output();
    match compile {
        Err(_) => false,
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if stderr.contains("release version") && stderr.contains("not supported") {
                    eprintln!(
                        "[osr_unbox_intrinsic_handler_frame] javac cannot target --release 21; \
                         skipping. Point JAVA_HOME at a JDK 21+ install."
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[osr_unbox_intrinsic_handler_frame] the checked-in fixture failed to compile \
                 — fix the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// `(stdout, stderr)`. `None` means a prerequisite was missing (and
/// `CRATONVM_REQUIRE_E2E` was not set).
fn run_probe() -> Option<(String, String)> {
    if !ensure_probe_compiled() {
        eprintln!(
            "[osr_unbox_intrinsic_handler_frame] fixture .class unavailable (javac on PATH?); \
             skipping"
        );
        return None;
    }
    let bin = cratonvm_binary()?;
    let mut child = match Command::new(&bin)
        // Both witnesses this test asserts on. They only print; neither changes
        // what is compiled.
        .env("CRATONVM_DBG_JITC", "1")
        .env("CRATONVM_DBG_ATOMIC_INTRINSIC", "1")
        .arg("-cp")
        .arg(probe_dir())
        .arg("UnboxPreOsrLocalProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[osr_unbox_intrinsic_handler_frame] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[osr_unbox_intrinsic_handler_frame] probe timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[osr_unbox_intrinsic_handler_frame] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "[osr_unbox_intrinsic_handler_frame] cratonvm exited {:?}\n--- stdout ---\n{stdout}",
        out.status.code(),
    );
    Some((stdout, stderr))
}

#[test]
fn an_intrinsic_claimed_site_inside_a_try_enters_the_handler_with_live_locals() {
    let Some((stdout, stderr)) = run_probe() else {
        return;
    };

    // ENGAGEMENT FIRST. A wrong answer is a defect; a right answer from a
    // configuration this test does not describe is worse, because it reads as
    // coverage. Both of these have a plausible way to stop being true without
    // anyone touching this file.
    // `OSR-compile UnboxPreOsrLocalProbe.arm()V` CONTIGUOUS, plus the absence of
    // a refusal. Both halves were measured, and the first spelling of this
    // assertion had neither: `l.contains("OSR-compile") &&
    // l.contains("...arm()V")` also matches
    // `OSR-compile FAILED ...arm()V ... method marked OSR-denied`, which is the
    // exact negative it exists to catch. With a `putstatic` planted inside the
    // fixture's `try` the run logs a `osr-DENY (osr-exc-site-unpublished pc=79
    // opcode=0xb3)`, compiles nothing, still prints the right answer, and that
    // assertion still passed.
    let osr_compiled = stderr
        .lines()
        .any(|l| l.contains("OSR-compile UnboxPreOsrLocalProbe.arm()V"));
    let osr_refused = stderr.lines().any(|l| {
        l.contains("UnboxPreOsrLocalProbe.arm()V")
            && (l.contains("osr-DENY") || l.contains("OSR-compile FAILED"))
    });
    assert!(
        osr_compiled && !osr_refused,
        "[osr_unbox_intrinsic_handler_frame] `arm()V` was not OSR-compiled (compiled={osr_compiled}, \
         refused={osr_refused}), so nothing here exercised the admission under test. Most likely \
         something throwing was added inside the fixture's `try` — \
         `first_unsupported_precise_frame_site` then refuses the whole method and the probe still \
         prints PASS. Check the fixture's four load-bearing properties before changing this \
         assertion.\n--- stderr (jitc lines) ---\n{}",
        jitc_lines(&stderr),
    );
    assert!(
        stderr.contains("[box-unbox-intrinsic] java/lang/Integer.intValue()I"),
        "[osr_unbox_intrinsic_handler_frame] the BOX_UNBOX intrinsic did not claim \
         `Integer.intValue()I`, which the fixture calls only from inside its `try`. The \
         substitution this test exists for did not happen — the family's default may have \
         been flipped off again.\n--- stderr (jitc lines) ---\n{}",
        jitc_lines(&stderr),
    );

    // Then the answer. `bad` is the handler's read of two locals set before the
    // loop; `drift` is its read of one the loop advances, which is what tells a
    // stale pre-OSR frame from a live one.
    assert!(
        stdout.contains(ORACLE),
        "[osr_unbox_intrinsic_handler_frame] the handler did not see what a real JDK sees.\n\
         want: {ORACLE}\n--- stdout ---\n{stdout}"
    );
    assert!(
        stdout.contains("PASS UnboxPreOsrLocalProbe"),
        "[osr_unbox_intrinsic_handler_frame] fixture reported failure\n--- stdout ---\n{stdout}"
    );
}

/// The `jitc`/intrinsic lines only — a failing assertion above wants the
/// compile decisions, not several megabytes of GC trace.
fn jitc_lines(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| l.contains("UnboxPreOsrLocalProbe") || l.contains("[box-unbox-intrinsic]"))
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
}
