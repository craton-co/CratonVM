// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 23 wave 2, lane L3: a `NullPointerException` raised by a null
//! receiver's field read inside a lambda impl's own `try`/`catch` must be
//! caught there, also once the impl's optimizing body superseded its first
//! one and the SAM call runs through the compiled direct lambda arm
//! (`jit::helpers::try_lambda_site_direct_call`).
//!
//! The optimizing tier reads the field through `jit_getfield`, whose null arm
//! raises the NPE flag WITHOUT the deopt flag; the direct arm's
//! `lambda_direct_sentinel_is_not_a_deopt` read that as "not a deopt" and
//! restashed the NPE for the compiled caller, past the impl's handler. See
//! `docs/feature-designs/jdk-only-mode.md`
//! (`CRATONVM_JIT_LAMBDA_IMPLICIT_WITHOUT_DEOPT_ROUTES`).
//!
//! Spawns the real `cratonvm` binary (the shape needs the tiers and the
//! background compiler); skipped when no binary or no class file is there.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("lambda_impl_npe_handler_fixtures")
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
        .parent()?
        .join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    ["release", "debug"]
        .iter()
        .map(|profile| target.join(profile).join(exe))
        .find(|candidate| candidate.exists())
}

fn run_fixture(extra_env: &[(&str, &str)]) -> Option<String> {
    let dir = fixture_dir();
    if !dir.join("LambdaImplNpeHandler.class").exists() {
        eprintln!("[lambda_impl_npe_handler] fixture class missing; skipping");
        return None;
    }
    let bin = cratonvm_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("-c")
        .arg(&dir)
        .arg("LambdaImplNpeHandler")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[lambda_impl_npe_handler] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let done = common::wait_draining(child, Duration::from_secs(120));
    assert!(!done.timed_out, "LambdaImplNpeHandler timed out");
    Some(format!(
        "{}\n--- STDERR ---\n{}",
        String::from_utf8_lossy(&done.output.stdout),
        String::from_utf8_lossy(&done.output.stderr)
    ))
}

/// Every rep catches every NPE inside the impl, and nothing escapes to
/// `main`, with the fix's switch at its default.
#[test]
fn an_npe_inside_a_lambda_impls_catch_is_caught_by_it() {
    let Some(out) = run_fixture(&[]) else {
        return;
    };
    assert!(
        !out.contains("Exception in thread"),
        "the NPE escaped the impl's catch:\n{out}"
    );
    assert!(!out.contains("caught=false"), "a rep lost NPEs:\n{out}");
    for rep in 0..10 {
        assert!(
            out.contains(&format!("rep={rep} caught=true")),
            "rep {rep} missing:\n{out}"
        );
    }
    assert!(out.contains("done"), "did not finish:\n{out}");
}
