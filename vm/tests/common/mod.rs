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

use std::path::PathBuf;

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
