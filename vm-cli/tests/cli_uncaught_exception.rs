// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end check that an uncaught Java exception escaping `main()`:
//!
//!   1. Causes cratonvm to exit non-zero (HotSpot does `exit(1)`).
//!   2. Renders the exception's class name AND at least one `\tat …`
//!      stack-trace frame on stderr (the canonical HotSpot format,
//!      mimicked by the cratonvm renderer in `vm-cli/src/main.rs`
//!      around line 1670-2180).
//!
//! The fixture `Thrower.class` is a one-liner that throws
//! `new RuntimeException("boom")` from `main`. Currently the cratonvm
//! exception renderer does not interpolate the message string into the
//! synthetic-JDK rendering (the `detailMessage` field walk at
//! `src/main.rs:1700-1727` runs but the field is empty in synthetic-JDK
//! mode), so we assert only on the exception's class name and the
//! `\tat Thrower.main(` frame — both of which the renderer reliably
//! emits today. The optional `"boom"` assertion is gated behind
//! `cfg(real_jdk_renderer)` / left as a TODO so a future fix that
//! populates `detailMessage` for synthetic Throwables doesn't have to
//! immediately update this test.

mod common;

use std::io::Read;

#[test]
fn uncaught_runtime_exception_exits_nonzero_with_stack_trace() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "Thrower");

    let mut cmd = common::cratonvm_cmd();
    cmd.arg("--classpath")
        .arg(tmp.path())
        .arg("Thrower")
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
        !status.success(),
        "expected non-zero exit for uncaught exception; status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
    );

    // Class name — internal-slash form is what the renderer emits today
    // for synthetic-JDK Throwables (`java/lang/RuntimeException`); the
    // dotted form is what a future "real JDK" renderer would emit.
    // Accept either so the test survives a renderer cleanup.
    let stderr_lc = stderr.to_lowercase();
    assert!(
        stderr_lc.contains("runtimeexception"),
        "expected RuntimeException class name in stderr; stderr={stderr:?}"
    );

    // At least one `\tat <something>.main(` frame.
    assert!(
        stderr.contains("\tat ") && stderr.contains("Thrower.main"),
        "expected a `\\tat Thrower.main(...)` stack-trace frame in stderr; stderr={stderr:?}"
    );
}
