// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end check that invoking cratonvm with a nonexistent main
//! class fails with a clear error and a non-zero exit code.
//!
//! HotSpot's behaviour for `java DoesNotExist` is:
//!
//!   Error: Could not find or load main class DoesNotExist
//!   Caused by: java.lang.ClassNotFoundException: DoesNotExist
//!   exit code 1
//!
//! cratonvm currently produces (verified manually 2026-05-24):
//!
//!   Could not find or load main class DoesNotExist: class file error: class not found: DoesNotExist
//!   exit code 1
//!
//! …which is close enough. This test asserts:
//!
//!   * exit code is non-zero (matches HotSpot's `exit(1)`),
//!   * stderr OR stdout contains the canonical "Could not find or load
//!     main class" phrase OR "ClassNotFoundException" (so a future
//!     renderer that switches to the HotSpot phrasing keeps passing).

mod common;

use std::io::Read;

#[test]
fn missing_main_class_fails_with_clear_error() {
    let mut cmd = common::cratonvm_cmd();
    cmd.arg("DoesNotExist")
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
        "missing class should fail; status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
    );

    let combined = format!("{stdout}{stderr}");
    let has_canonical_phrase = combined.contains("Could not find or load main class")
        || combined.contains("ClassNotFoundException")
        || combined.contains("class not found");
    assert!(
        has_canonical_phrase,
        "expected a clear missing-class error message on stdout or stderr; \
         stdout={stdout:?}, stderr={stderr:?}"
    );

    // The unresolved class name should appear somewhere in the diagnostic.
    assert!(
        combined.contains("DoesNotExist"),
        "expected the unresolved class name 'DoesNotExist' in the diagnostic; \
         stdout={stdout:?}, stderr={stderr:?}"
    );
}
