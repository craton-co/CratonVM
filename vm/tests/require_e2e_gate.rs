// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `CRATONVM_REQUIRE_E2E` must be read by the harness AND set by a workflow.
//!
//! # The defect this exists to prevent
//!
//! A flag no consumer reads and a flag no producer sets are the same defect
//! seen from two ends, and this repository has produced both — most recently a
//! young-generation pause goal that moved a number nothing read. The gate here
//! was the second kind. `vm/tests/common/mod.rs` promotes a missing binary, a
//! missing JDK or a missing fixture to a panic when `CRATONVM_REQUIRE_E2E` is
//! set, and its own module docs said *"CI sets it to assert that a green run
//! was a real one"* — while **no file under `.github/workflows/` mentioned the
//! variable at all**. Every `require_binary` / `require_jdk` / `require_fixture`
//! skip was therefore a silent green in CI, exactly as if the helper had never
//! been written. Recorded as W6-5-vacuous-tests.md §3.3.
//!
//! # The two halves, asserted separately
//!
//! * [`the_gate_is_honoured_by_the_harness`] drives the CONSUMER. It calls the
//!   real `common::require_fixture` / `common::require_binary` with the
//!   variable set and unset and asserts the behaviour differs. Deleting the
//!   `env::var` read in `common/mod.rs`, or making `require_e2e()` return a
//!   constant, turns this red.
//! * [`a_workflow_sets_the_gate`] drives the PRODUCER. It reads
//!   `.github/workflows/*.yml` and fails when none of them sets the variable.
//!   Deleting the CI step turns this red.
//!
//! Neither half is worth much alone: a consumer test alone is what the
//! situation already was (a correct helper nothing switched on), and a workflow
//! grep alone would pass against a helper that had stopped reading the value.
//!
//! # Placement and prerequisites
//!
//! This file needs no `cratonvm` binary, no JDK and no Java fixture, so it runs
//! in a plain `cargo test --workspace` on any machine — which is the point: the
//! thing being asserted is that a green run was a real one, and a check that
//! itself skips could not say that.

use std::path::{Path, PathBuf};

mod common;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

/// A path that cannot exist, so `require_fixture` always takes its miss path.
fn a_path_that_cannot_exist() -> PathBuf {
    workspace_root()
        .join("apps")
        .join("no-such-probe-9f3c1d")
        .join("NoSuchProbe.java")
}

/// Run `f`, returning `true` if it panicked. The panic hook is muted for the
/// duration so an EXPECTED panic does not print a backtrace that reads like a
/// failure in the test log.
fn panicked(f: impl FnOnce() + std::panic::UnwindSafe) -> bool {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(f);
    std::panic::set_hook(previous);
    outcome.is_err()
}

/// The CONSUMER half: the harness must actually branch on the variable.
///
/// Deliberately ONE test function rather than four. `set_var` is process-wide
/// and cargo runs a binary's tests on parallel threads, so splitting this would
/// make the halves race each other and produce a flake whose green runs proved
/// nothing — the shape this whole record is about.
#[test]
fn the_gate_is_honoured_by_the_harness() {
    let var = common::REQUIRE_VAR;
    let saved = std::env::var(var).ok();
    let absent = a_path_that_cannot_exist();
    assert!(
        !absent.exists(),
        "the negative-control path {} exists; pick another",
        absent.display()
    );

    // --- gate OFF: the historical skip, byte for byte -----------------------
    std::env::remove_var(var);
    assert!(!common::require_e2e(), "unset must read as not-demanded");
    assert!(
        common::require_fixture(
            "gate-selftest",
            "a fixture that cannot exist",
            &[absent.clone()]
        )
        .is_none(),
        "with the gate off a missing fixture must return None, not panic"
    );
    assert!(
        common::require_binary(None).is_none(),
        "with the gate off a missing binary must return None, not panic"
    );

    // `0` and the empty string are documented as off-switches. They are the
    // only reason an unset/set pair is not enough to characterise this.
    std::env::set_var(var, "0");
    assert!(!common::require_e2e(), "`0` must read as not-demanded");
    std::env::set_var(var, "");
    assert!(!common::require_e2e(), "empty must read as not-demanded");
    assert!(
        common::require_fixture(
            "gate-selftest",
            "a fixture that cannot exist",
            &[absent.clone()]
        )
        .is_none(),
        "the empty string is an off-switch, so a missing fixture must still return None"
    );

    // --- gate ON: every missing prerequisite becomes a failure --------------
    std::env::set_var(var, "1");
    assert!(common::require_e2e(), "`1` must read as demanded");
    let a = absent.clone();
    assert!(
        panicked(move || {
            let _ = common::require_fixture("gate-selftest", "a fixture that cannot exist", &[a]);
        }),
        "with {var} set, a MISSING FIXTURE must panic. It did not, so every fixture-gated test in \
         this suite is still a silent green under the gate — which is the whole defect \
         W6-5-vacuous-tests.md §3.3 recorded."
    );
    assert!(
        panicked(|| {
            let _ = common::require_binary(None);
        }),
        "with {var} set, a MISSING BINARY must panic"
    );
    assert!(
        panicked(|| {
            let _ = common::require_jdk(None);
        }),
        "with {var} set, a MISSING JDK must panic"
    );

    // A present fixture must still resolve under the gate — without this, a
    // `require_fixture` that panicked unconditionally would satisfy every
    // assertion above.
    let present = workspace_root().join("Cargo.toml");
    assert!(present.exists(), "the positive control must exist");
    assert_eq!(
        common::require_fixture(
            "gate-selftest",
            "a file that does exist",
            &[present.clone()]
        ),
        Some(present),
        "with the gate set, a fixture that IS present must be returned, not refused"
    );

    match saved {
        Some(v) => std::env::set_var(var, v),
        None => std::env::remove_var(var),
    }
}

/// The PRODUCER half: some workflow has to switch the gate on.
#[test]
fn a_workflow_sets_the_gate() {
    let var = common::REQUIRE_VAR;
    let dir = workspace_root().join(".github").join("workflows");
    assert!(
        dir.is_dir(),
        "{} is not a directory; this guard cannot see the workflows it is about",
        dir.display()
    );

    let mut scanned: Vec<String> = Vec::new();
    let mut setters: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok);
    for entry in entries {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yml" && ext != "yaml" {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        scanned.push(name.clone());
        // Both spellings a workflow can use: an `env:` mapping entry
        // (`CRATONVM_REQUIRE_E2E: 1`) or an inline assignment in a `run:`
        // line (`CRATONVM_REQUIRE_E2E=1 cargo test ...`). A bare mention in a
        // comment is NOT a setter and must not satisfy this.
        for line in src.lines() {
            let code = match line.find('#') {
                Some(i) => &line[..i],
                None => line,
            };
            let Some(at) = code.find(var) else { continue };
            let rest = code[at + var.len()..].trim_start();
            if rest.starts_with(':') || rest.starts_with('=') {
                setters.push(name.clone());
                break;
            }
        }
    }

    assert!(
        !scanned.is_empty(),
        "no workflow files were scanned; the guard is looking in the wrong place"
    );
    assert!(
        !setters.is_empty(),
        "[require-e2e-gate] no file under .github/workflows/ sets {var} (scanned {}: {}).\n\nThe \
         helpers in `vm/tests/common/mod.rs` promote a missing binary / JDK / fixture to a failure \
         ONLY when it is set, so with no producer every skipped end-to-end test is a silent green \
         in CI and the helpers are decoration. Either add a step that exports it, or delete the \
         helpers and stop claiming a gate exists. See W6-5-vacuous-tests.md §3.3 and \
         W7-51-vacuous-sweep-round-2.md.",
        scanned.len(),
        scanned.join(", ")
    );
}

/// The consumer source must still READ the variable.
///
/// The behavioural test above already proves this on a machine that runs it;
/// this is the cheap static half, and it names the file so a reader who has
/// just deleted the read knows what the failure is about.
#[test]
fn the_harness_source_still_reads_the_gate() {
    let src_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("common")
        .join("mod.rs");
    let src = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", src_path.display()));
    assert!(
        src.contains(common::REQUIRE_VAR),
        "[require-e2e-gate] {} no longer mentions {}; the consumer side of the gate is gone",
        src_path.display(),
        common::REQUIRE_VAR
    );
    assert!(
        src.contains("env::var(REQUIRE_VAR)"),
        "[require-e2e-gate] {} no longer reads {} from the environment. A gate that reads a \
         constant is a gate that never fires.",
        src_path.display(),
        common::REQUIRE_VAR
    );
}
