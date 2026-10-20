// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end check that program arguments after the main class are
//! delivered to `public static void main(String[] args)` in order.
//!
//! This exercises the full positional pipeline:
//!
//!   `insert_program_args_separator` → clap parsing → `--` strip in
//!   `run()` → `args.args` vec → `create_java_string` for each → boxed
//!   `Object[]` array passed to `main` as the single
//!   `[Ljava/lang/String;` parameter.
//!
//! `PrintArgs.class` (compiled from the .java source in
//! `tests/resources/PrintArgs.java`) prints each `args[i]` on its own
//! line, so the expected stdout is the exact argv `\n`-joined.
//!
//! Bonus: this is the only e2e coverage that touches the args-array
//! element type. `run()` now resolves the real `java/lang/String`
//! ClassId via `load_class_concurrent` (falling back to `ClassId::new(0)`
//! only if that load fails), so `main(String[])` receives a proper
//! `[Ljava/lang/String;` rather than a `[Ljava/lang/Object;`. Downstream
//! uses that inspect `args.getClass().getComponentType()` or call
//! `args[i].length()` (not exercised here, but a follow-up test could
//! add it and check the printed integer) therefore work — this test
//! just guards the argv plumbing itself.

mod common;

use std::io::Read;

#[test]
fn main_args_delivered_in_order() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "PrintArgs");

    let mut cmd = common::cratonvm_cmd();
    cmd.arg("--classpath")
        .arg(tmp.path())
        .arg("PrintArgs")
        .arg("alpha")
        .arg("beta")
        .arg("gamma")
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
        "cratonvm exited with {status:?}; stdout was {stdout:?}"
    );

    // PrintArgs prints exactly one line per arg, in argv order.
    // Use line-by-line comparison to be tolerant of any leading/trailing
    // diagnostic prints (there should not be any on stdout, but we
    // don't want a benign tracing line to flake the test).
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.windows(3).any(|w| w == ["alpha", "beta", "gamma"]),
        "expected alpha/beta/gamma to appear in order on stdout; got lines={lines:?}"
    );
}
