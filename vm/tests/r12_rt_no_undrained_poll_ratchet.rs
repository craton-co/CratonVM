// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: no `vm/tests` harness may poll a child that pipes its output
//! without draining the pipes while it waits.
//!
//! The shape this forbids spawns the VM with `Stdio::piped()` stdout and
//! stderr, polls `try_wait` in a sleep loop, and reads the pipes only after
//! the child has exited (`wait_with_output`). An OS pipe holds 64 KiB (Rust's
//! std asks for that on Windows too). A child that writes more to either
//! stream blocks in `write` and never exits, so the harness waits out its
//! whole timeout and reports a hang the VM does not have. That cost a wave-36
//! investigation (`w36-osr-athrow-lift-test-hangs-intermittently-under-the-harness`)
//! and is written up above `common::wait_draining`, which is the fix.
//!
//! Round 12 wave 2 (lane rt) migrated the 67 files that had it
//! (`r12w1-osr-vm-tests-poll-loop-never-drains-piped-output-patch`, proposal 3
//! of `docs/internal/jit-proposals/jit-r12-osr-proposals-RETIRED-20260928.md`), so the frozen count is zero.
//!
//! # The rule
//!
//! With all whitespace removed, a file counts when it contains a
//! `.try_wait()` call and a `Stdio::piped()` redirect, and none of these
//! drains: a `wait_draining(` or `wait_watching(` call (`common/mod.rs`), or a
//! `stdout.take()` / `stderr.take()` of the child's pipe (a hand-rolled
//! reader thread, as `jit_ir_athrow_dispatch.rs` has). The check is per file:
//! it cannot see a second, undrained loop in a file that drains once. This
//! file's own test snippets include a drain, so it never counts itself.

use std::path::{Path, PathBuf};

/// Files allowed to keep the undrained poll. Lower it, never raise it.
const FROZEN_UNDRAINED: usize = 0;

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn squash(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `(poll, piped, drains)` needles, built at runtime.
fn needles() -> (String, String, Vec<String>) {
    let poll = format!(".{}{}()", "try_", "wait");
    let piped = format!("Stdio::{}()", "piped");
    let drains = vec![
        format!("{}_{}(", "wait", "draining"),
        format!("{}_{}(", "wait", "watching"),
        format!("stdout.{}()", "take"),
        format!("stderr.{}()", "take"),
    ];
    (poll, piped, drains)
}

fn polls_undrained(src: &str) -> bool {
    let (poll, piped, drains) = needles();
    let s = squash(src);
    s.contains(&poll) && s.contains(&piped) && !drains.iter().any(|d| s.contains(d.as_str()))
}

#[test]
fn no_vm_test_polls_a_child_whose_pipes_nobody_drains() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();
    assert!(
        files.len() > 100,
        "found only {} sources under {}; the walk is broken, not the suite",
        files.len(),
        root.display()
    );

    let (_, _, drains) = needles();
    let mut undrained = Vec::new();
    let mut draining = 0usize;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if squash(&src).contains(drains[0].as_str()) {
            draining += 1;
        }
        if polls_undrained(&src) {
            let shown = path.strip_prefix(&root).unwrap_or(path.as_path());
            undrained.push(shown.display().to_string());
        }
    }
    // Anti-vacuity: the migrated files call `wait_draining`, so a needle that
    // no longer matches anything shows up here first.
    assert!(
        draining >= 60,
        "only {draining} files call wait_draining; the needles no longer match the suite"
    );
    assert!(
        undrained.len() <= FROZEN_UNDRAINED,
        "{} vm/tests file(s) poll `try_wait` on a child with piped output and never drain \
         the pipes while waiting (frozen at {FROZEN_UNDRAINED}):\n  {}\n\
         Replace the loop with `common::wait_draining(child, <timeout>)` (or \
         `common::wait_watching` for a probe that reports progress); see the \
         comment above `wait_draining` in vm/tests/common/mod.rs.",
        undrained.len(),
        undrained.join("\n  ")
    );
    assert_eq!(
        undrained.len(),
        FROZEN_UNDRAINED,
        "fewer undrained files than frozen: lower FROZEN_UNDRAINED to {}",
        undrained.len()
    );
}

#[test]
fn the_predicate_flags_only_the_undrained_shape() {
    let undrained = "cmd.stdout(Stdio::piped());\nlet mut child = cmd.spawn()?;\n\
                     loop { match child\n    .try_wait() { _ => break } }\n\
                     let out = child.wait_with_output()?;";
    assert!(polls_undrained(undrained));
    let drained = "cmd.stdout(Stdio::piped());\nlet child = cmd.spawn()?;\n\
                   let done = common::wait_draining(child, T);";
    assert!(!polls_undrained(drained));
    let hand_rolled = "cmd.stdout(Stdio::piped());\nlet out = child\n    .stdout\n    .take();\n\
                       loop { match child.try_wait() { _ => break } }";
    assert!(!polls_undrained(hand_rolled));
    let comment_only = "// This used to `try_wait()` in a loop.\ncmd.stdout(Stdio::piped());";
    assert!(!polls_undrained(comment_only), "a comment is not a poll");
    let not_piped = "cmd.stdout(Stdio::inherit());\nloop { child.try_wait(); }";
    assert!(!polls_undrained(not_piped));
}
