// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end smoke test: `cratonvm --classpath <dir> HelloWorld` prints
//! "Hello, World!" and exits 0.
//!
//! This is the most basic vm-cli e2e test — every other `cli_*.rs` test
//! depends implicitly on this path working, so a regression here flags
//! the whole launcher orchestration (clap parsing, classpath assembly,
//! `Vm::new`, `System.initPhase1`, `main(String[])` invocation, normal
//! exit). Stdout is asserted exactly to catch any stray
//! `[DIAG-*]`/`[TRACE-*]`/print-debug regression on the println path.
//! Stderr is NOT asserted empty because the default watchdog prints a
//! one-line banner before main runs.

mod common;

use std::io::Read;

#[test]
fn helloworld_classpath_dir_runs_and_exits_zero() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "HelloWorld");

    let mut cmd = common::cratonvm_cmd();
    cmd.arg("--classpath")
        .arg(tmp.path())
        .arg("HelloWorld")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let mut stdout = String::new();
    child
        .stdout
        .as_mut()
        .expect("stdout pipe")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let status = child.wait().expect("wait for cratonvm");

    assert!(
        status.success(),
        "cratonvm exited with {status:?}; stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected 'Hello, World!' in stdout, got {stdout:?}"
    );
}
