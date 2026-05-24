//! JIT kill-switch end-to-end check.
//!
//! There is currently NO `--nojit` CLI flag on the cratonvm launcher
//! (see `.claude/review-2026-05-24/vm-cli.md` §3 — "README inaccuracy"
//! row: the README's "Command-Line Reference" table lists `--nojit` but
//! `grep nojit vm-cli/src/main.rs` returns 0 hits). The interpreter
//! does read the `CRATONVM_DISABLE_JIT` environment variable
//! (`vm/src/runtime/env_cache.rs::disable_jit`), so this test verifies
//! end-to-end that:
//!
//!   1. Setting `CRATONVM_DISABLE_JIT=1` on the spawned process is
//!      honoured (HelloWorld still runs to a clean exit) — proving the
//!      env-var path that the eventual `--nojit` flag will rewrite to.
//!   2. The CURRENTLY-MISSING `--nojit` CLI flag is rejected by clap
//!      with the expected `unexpected argument` error. This is a
//!      regression marker: the day someone wires up `--nojit` to set
//!      `CRATONVM_DISABLE_JIT=1`, the second test starts failing and
//!      the first test should be extended to also assert behaviour via
//!      the new flag.

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

/// Regression marker for the missing `--nojit` CLI flag. Today this
/// test passes because clap rejects the flag; once `--nojit` is added
/// to `Args` (mapped to `env::set_var("CRATONVM_DISABLE_JIT", "1")` at
/// the top of `run()`), this test will start failing — that's the
/// signal to extend `cratonvm_disable_jit_env_var_runs_helloworld`
/// above to cover the flag path too, then delete this test.
#[test]
fn nojit_cli_flag_currently_absent() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "HelloWorld");

    let mut cmd = common::cratonvm_cmd();
    cmd.arg("--nojit")
        .arg("--classpath")
        .arg(tmp.path())
        .arg("HelloWorld")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let mut stderr = String::new();
    child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().expect("wait for cratonvm");

    if status.success() {
        panic!(
            "`--nojit` is now accepted by clap; wire `cratonvm_disable_jit_env_var_runs_helloworld` \
             to also assert via the flag and delete this regression-marker test."
        );
    }
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("--nojit"),
        "expected clap to reject `--nojit`; stderr was {stderr:?}"
    );
}
