// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Source-level guard: a test that compiles a Java probe must FAIL when that
//! probe stops compiling — never silently skip.
//!
//! # The defect this exists to prevent
//!
//! Most end-to-end tests under `vm/tests/` compile a Java probe (either an
//! embedded `PROBE_SRC` string or a checked-in `.java` fixture) and then run it
//! under the `cratonvm` binary. Every one of them needs a skip path, because CI
//! images without a JDK must not fail the suite. The trap is that the obvious
//! way to write that skip collapses two very different outcomes:
//!
//! * `javac` is absent / cannot be launched — a legitimate skip
//! * `javac` RAN and REJECTED the source — the probe is broken, and skipping
//!   turns the test into a **permanent vacuous pass** that can never fail again
//!
//! This is not hypothetical. On 2026-08-01 an edit to
//! `sealed_bootstrap_permitted_subclasses.rs` shadowed two locals in its
//! `PROBE_SRC`; javac errored, the helper returned `None`, the caller printed
//! "javac unavailable; skipping", and the test reported **`ok` in 0.8 s** while
//! running against a binary that reproduced the bug it was written to catch. It
//! was caught only by mutation-checking the test against a known-bad binary.
//! A sub-second "ok" from a test that must boot a whole VM is the tell.
//!
//! # What this guard checks
//!
//! Every `vm/tests/*.rs` that launches a Java compiler must contain an
//! assertion whose message includes [`REQUIRED_MARKER`]. That is a *positive*
//! requirement rather than a blocklist of bad shapes, because the bad shapes
//! are many (`.ok()?`, `.then_some(..)`, `matches!(status, Ok(s) if
//! s.success())`, `_ => false`, `eprintln!("javac failed; skipping")`) and new
//! ones are easy to invent, while the good shape always ends the same way:
//! panic, and put javac's stderr in the message so the break is diagnosable
//! from CI output alone.
//!
//! The canonical shape:
//!
//! ```ignore
//! let out = match Command::new(javac).args(..).output() {
//!     Ok(o) => o,
//!     // javac cannot be launched at all — the one legitimate skip.
//!     Err(e) => { eprintln!("[tag] javac could not be executed: {e}; skipping"); return None; }
//! };
//! assert!(
//!     out.status.success(),
//!     "[tag] the embedded probe failed to compile — fix the probe source. javac stderr:\n{}",
//!     String::from_utf8_lossy(&out.stderr)
//! );
//! ```
//!
//! Note `.output()`, not `.status()`: `.status()` lets javac's diagnostics go
//! to the test harness's captured stderr, where a `cargo test` summary will not
//! show them.

use std::path::{Path, PathBuf};

/// Substring every probe-compiling test's failure assertion must contain.
const REQUIRED_MARKER: &str = "failed to compile";

/// Files that legitimately launch a Java compiler but do not need
/// [`REQUIRED_MARKER`], each with the reason. Keep this list short and
/// justified — an entry here is a test that can go quiet.
const ALLOWED: &[(&str, &str)] = &[
    (
        "probe_compile_guard.rs",
        "this guard itself — it reads sources, it does not compile probes",
    ),
    (
        "differential.rs",
        "surfaces javac's stderr as an `error:javac failed: ..` Outcome that the \
         differential comparison then reports; it never silently skips",
    ),
    (
        "t4_9_real_app_conformance.rs",
        "runs javac AS the subject under test (T4.9.12 javac self-host) rather than \
         to compile a probe; a javac failure there is the result, not a skip",
    ),
];

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Does this source launch a Java compiler? Covers `Command::new("javac")`,
/// `Command::new(javac)` / `Command::new(&javac)` / `Command::new(javac_path(..))`
/// and the `java --module jdk.compiler/com.sun.tools.javac.Main` fallback.
fn launches_a_java_compiler(src: &str) -> bool {
    let compiles = [
        "Command::new(\"javac\")",
        "Command::new(javac",
        "Command::new(&javac",
        "Command::new(javac_path",
        "Command::new(javac_bin",
        "Command::new(javac_executable",
        "jdk.compiler/com.sun.tools.javac.Main",
    ];
    // `Command::new(` followed by an expression that builds `bin/javac` also
    // counts — e.g. `Command::new(java_home.join("bin").join("javac.exe"))`.
    compiles.iter().any(|needle| src.contains(needle))
        || (src.contains("Command::new(") && src.contains("\"javac.exe\""))
}

#[test]
fn every_probe_compiling_test_fails_on_a_compile_error() {
    let dir = tests_dir();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok);

    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if ALLOWED.iter().any(|(f, _)| *f == name) {
            continue;
        }
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => panic!("read {}: {e}", path.display()),
        };
        if !launches_a_java_compiler(&src) {
            continue;
        }
        checked += 1;
        if !src.contains(REQUIRED_MARKER) {
            offenders.push(name);
        }
    }

    // If this drops to zero the detector has stopped matching anything (a
    // rename of `Command::new`, a move to a shared helper) and the guard is
    // vacuous — exactly the failure mode it exists to prevent.
    assert!(
        checked >= 20,
        "[probe_compile_guard] only matched {checked} probe-compiling tests; the \
         detector in `launches_a_java_compiler` has gone stale and this guard is \
         no longer guarding anything. Fix the detector, do not lower this bound."
    );

    assert!(
        offenders.is_empty(),
        "[probe_compile_guard] {} test(s) launch javac but have no assertion \
         containing {REQUIRED_MARKER:?}, so a probe that stops compiling makes them \
         report `ok` forever:\n  {}\n\nFix: keep the skip for a javac that cannot be \
         LAUNCHED, and `assert!` on `out.status.success()` with \
         `String::from_utf8_lossy(&out.stderr)` in the message when javac RAN and \
         rejected the source. See this file's module docs for the canonical shape, \
         or `sealed_bootstrap_permitted_subclasses.rs` for a worked example.",
        offenders.len(),
        offenders.join("\n  ")
    );
}
