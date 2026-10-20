// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT kill-switch end-to-end check.
//!
//! The cratonvm launcher exposes a `--nojit` boolean flag (see
//! `vm-cli/src/main.rs` — `Args.nojit` and the post-parse block that
//! sets `CRATONVM_DISABLE_JIT=1` so the existing kill-switch in
//! `vm/src/runtime/env_cache.rs::disable_jit` observes it on first
//! read). This test verifies end-to-end that:
//!
//!   1. Setting `CRATONVM_DISABLE_JIT=1` on the spawned process is
//!      honoured (HelloWorld still runs to a clean exit) — the env-var
//!      path the `--nojit` flag itself rewrites to.
//!   2. The `--nojit` CLI flag is accepted by clap and produces the
//!      same end-to-end behaviour as the env var (HelloWorld runs to a
//!      clean exit). This guards the post-parse `set_var` wiring in
//!      `run()` from regression.

mod common;

use std::io::Read;

#[test]
fn cratonvm_disable_jit_env_var_runs_helloworld() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "HelloWorld");

    let mut cmd = common::cratonvm_cmd();
    cmd.env("CRATONVM_DISABLE_JIT", "1")
        .arg("--classpath")
        .arg(tmp.path())
        .arg("HelloWorld")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let mut stdout = String::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let status = child.wait().expect("wait for cratonvm");

    assert!(
        status.success(),
        "cratonvm with CRATONVM_DISABLE_JIT=1 should still run; status={status:?}, stdout={stdout:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected 'Hello, World!' in stdout, got {stdout:?}"
    );
}

/// The `--nojit` CLI flag is accepted by clap and rewrites to the
/// `CRATONVM_DISABLE_JIT=1` env var inside `run()` before any
/// interpreter / JIT dispatcher reads `env_cache::disable_jit()`. End
/// to end, an invocation with `--nojit` runs HelloWorld to a clean
/// exit just like the env-var path above, and clap must not emit any
/// "unexpected argument" diagnostic for the flag.
#[test]
fn nojit_cli_flag_runs_helloworld() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "HelloWorld");

    let mut cmd = common::cratonvm_cmd();
    // Scrub any ambient CRATONVM_DISABLE_JIT so this test only exercises
    // the `--nojit` -> set_var path inside run(), not an inherited env.
    cmd.env_remove("CRATONVM_DISABLE_JIT")
        .arg("--nojit")
        .arg("--classpath")
        .arg(tmp.path())
        .arg("HelloWorld")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().expect("wait for cratonvm");

    assert!(
        status.success(),
        "cratonvm --nojit should be accepted and run to a clean exit; \
         status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "clap should accept --nojit; stderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected 'Hello, World!' in stdout, got {stdout:?}"
    );
}
