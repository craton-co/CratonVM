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

//! # The second gap, closed 2026-08-07
//!
//! The guard above checks the compile-FAILURE path. Every finding of the
//! 2026-08-07 vacuous-test audit lived on the compile-*MISSING* path instead:
//! the fixture was not there to compile at all. 23 distinct `apps/<probe>/`
//! fixtures referenced from `vm/tests/*.rs` were absent from the tree, ~60
//! tests skipped on them, and this guard passed the whole time — a fixture that
//! does not exist never reaches javac, so it never produces a compile error for
//! `REQUIRED_MARKER` to be about.
//!
//! The root cause is structural: `.gitignore` line 12 is `apps/`, so a fixture
//! written under `apps/` is untracked and vanishes for every other checkout.
//! `every_apps_fixture_reference_reports_a_missing_fixture` below therefore
//! requires that any test constructing a path into that gitignored tree route
//! its missing-fixture case through [`common::require_fixture`], which makes
//! the skip loud and makes `CRATONVM_REQUIRE_E2E=1` fail it.
//!
//! It is a positive requirement for the same reason the older check is: the bad
//! shapes are many (`if !dir.exists() { return; }`, `return false`, `return
//! None`, a bare `eprintln!`) and easy to reinvent, while the good shape always
//! names the same helper.
//!
//! Two standing lessons are honoured here:
//!
//! * **No fixed line bands.** Everything keys on content, never on line
//!   numbers, so the checks do not go stale when a file grows.
//! * **CRLF-safe.** Every needle is a single-line substring, so a Windows
//!   checkout's `\r\n` never splits one.

use std::path::{Path, PathBuf};

/// Substring every probe-compiling test's failure assertion must contain.
const REQUIRED_MARKER: &str = "failed to compile";

/// Substring every test that reaches into the gitignored `apps/` tree must
/// contain: the name of the shared missing-fixture reporter in
/// `vm/tests/common/mod.rs`.
const FIXTURE_MARKER: &str = "require_fixture";

/// Files that build a path under `apps/` but do not need [`FIXTURE_MARKER`],
/// each with the reason. Keep this list short and justified — an entry here is
/// a test that can go quiet when its fixture disappears.
const FIXTURE_ALLOWED: &[(&str, &str)] = &[
    (
        "probe_compile_guard.rs",
        "this guard itself — the `apps/` needles below are the detector, not a fixture lookup",
    ),
    (
        "fjp_recursive.rs",
        "stricter than require_fixture: `probe_source()` PANICS on a missing fixture \
         unconditionally, not only under CRATONVM_REQUIRE_E2E",
    ),
    (
        "rfjp1_recursive.rs",
        "same as fjp_recursive.rs — its `probe_source()` panics rather than skipping",
    ),
    (
        "probe_fixture_census.rs",
        "the fixture ratchet — like this guard, its `apps/` literals are a detector, not a \
         lookup. It is the check that makes a missing fixture fail a DEFAULT `cargo test` run \
         rather than only a `CRATONVM_REQUIRE_E2E=1` one",
    ),
];

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

/// Does this source construct a path into the gitignored `apps/` tree?
///
/// Keyed on the *code* forms only — `join("apps")`, `join("apps/…")`,
/// `PathBuf::from("apps…")`, `Path::new("apps…")`. Deliberately NOT on the bare
/// text `apps/`, which appears in the prose of a dozen module doc comments that
/// never touch the directory (`sublist_view_regression.rs`,
/// `getclass_concrete_class.rs`, `t8_deprecated_conformance.rs`, …). Flagging
/// those would train the next reader to add allow-list entries instead of
/// fixing anything.
fn references_a_gitignored_apps_fixture(src: &str) -> bool {
    let needles = [
        "join(\"apps\")",
        "join(\"apps/",
        "join(\"apps\\\\",
        "PathBuf::from(\"apps",
        "Path::new(\"apps",
    ];
    needles.iter().any(|needle| src.contains(needle))
}

/// Read every `vm/tests/*.rs`, skipping the given allow-list, and hand each
/// `(file name, source)` pair to `f`. Shared by both guards so a rename of the
/// tests directory breaks them together rather than silently disabling one.
fn for_each_test_source(allowed: &[(&str, &str)], mut visit: impl FnMut(&str, &str)) {
    let dir = tests_dir();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok);
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if allowed
            .iter()
            .any(|(allowed_name, _)| *allowed_name == name)
        {
            continue;
        }
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => panic!("read {}: {e}", path.display()),
        };
        visit(&name, &src);
    }
}

/// A test that reaches into the gitignored `apps/` tree must report a MISSING
/// fixture through `common::require_fixture`.
///
/// See this file's module docs for why this is the gap the older guard leaves
/// open, and why the requirement is positive rather than a blocklist.
#[test]
fn every_apps_fixture_reference_reports_a_missing_fixture() {
    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for_each_test_source(FIXTURE_ALLOWED, |name, src| {
        if !references_a_gitignored_apps_fixture(src) {
            return;
        }
        checked += 1;
        if !src.contains(FIXTURE_MARKER) {
            offenders.push(name.to_string());
        }
    });

    // If this drops to zero the detector has stopped matching anything (a move
    // to a shared path helper, a rename of the fixture root) and the guard is
    // vacuous — exactly the failure mode it exists to prevent. The floor is
    // deliberately well under the ~30 files that matched on 2026-08-07, so
    // legitimately deleting fixtures does not force an edit here; it is a
    // detector-liveness check, not a census.
    assert!(
        checked >= 15,
        "[probe_compile_guard] only matched {checked} test(s) that reach into the gitignored \
         `apps/` tree; the detector in `references_a_gitignored_apps_fixture` has gone stale and \
         this guard is no longer guarding anything. Fix the detector, do not lower this bound."
    );

    assert!(
        offenders.is_empty(),
        "[probe_compile_guard] {} test(s) build a path under the gitignored `apps/` tree but \
         never call `common::{FIXTURE_MARKER}`, so when the fixture is absent — which is the \
         NORMAL state, since `.gitignore` line 12 makes everything under `apps/` untracked — \
         they return early and report `ok` while asserting nothing:\n  {}\n\nFix: keep the early \
         return, but report the miss first:\n\n    if !src.exists() {{\n        let _ = \
         common::require_fixture(\"tag\", \"what is missing\", &[src.clone()]);\n        return \
         false;\n    }}\n\nThat makes the skip LOUD and makes `CRATONVM_REQUIRE_E2E=1` fail it. \
         See `vm/tests/common/mod.rs`. If a file genuinely does not need it (because it panics \
         outright, as `fjp_recursive.rs` does), add it to `FIXTURE_ALLOWED` with the reason.",
        offenders.len(),
        offenders.join("\n  ")
    );
}

#[test]
fn every_probe_compiling_test_fails_on_a_compile_error() {
    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for_each_test_source(ALLOWED, |name, src| {
        if !launches_a_java_compiler(src) {
            return;
        }
        checked += 1;
        if !src.contains(REQUIRED_MARKER) {
            offenders.push(name.to_string());
        }
    });

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
