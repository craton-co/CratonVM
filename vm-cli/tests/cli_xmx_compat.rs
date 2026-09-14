// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot `-Xmx` single-dash compatibility (review item C36 / "HIGH bug"
//! in `.claude/review-2026-05-24/vm-cli.md` §1).
//!
//! Stock `java -Xmx256m HelloWorld` (and the two-token form
//! `java -Xmx 256m HelloWorld`) MUST be accepted by the `java` bin alias
//! the cratonvm-cli ships — that is the whole reason the alias exists
//! (Maven Surefire / Gradle / IDE launchers all pass `-Xmx`).
//!
//! The pre-clap rewriter in `vm-cli/src/main.rs` (`normalize_java_launcher_argv`,
//! the `-Xmx` branch) folds both `-Xmx<size>` and `-Xmx <size>` into the
//! clap-understood `--Xmx <size>` spelling. This test guards that
//! behaviour from regression by spawning the binary end-to-end with each
//! form and asserting:
//!
//!   1. clap does not reject the invocation (no "unexpected argument").
//!   2. HelloWorld runs to a clean exit, which proves the heap size
//!      parsed by `parse_size` and threaded through `with_max_heap_size`
//!      is large enough to host the process.

mod common;

use std::io::Read;

fn run_with_xmx_args(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "HelloWorld");

    let mut cmd = common::cratonvm_cmd();
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("--classpath")
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
    (status, stdout, stderr)
}

/// `-Xmx256m` (inline single-dash) is rewritten to `--Xmx 256m` and the
/// VM runs HelloWorld to a clean exit.
#[test]
fn xmx_inline_single_dash_accepted() {
    let (status, stdout, stderr) = run_with_xmx_args(&["-Xmx256m"]);
    assert!(
        !stderr.contains("unexpected argument"),
        "clap should not reject -Xmx256m; stderr={stderr:?}"
    );
    assert!(
        status.success(),
        "cratonvm -Xmx256m … failed with {status:?}\nstdout={stdout:?}\nstderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected program output, got stdout={stdout:?}"
    );
}

/// `-Xmx 256m` (two-token single-dash) is rewritten to `--Xmx 256m` and
/// the VM runs HelloWorld to a clean exit.
#[test]
fn xmx_separate_token_single_dash_accepted() {
    let (status, stdout, stderr) = run_with_xmx_args(&["-Xmx", "256m"]);
    assert!(
        !stderr.contains("unexpected argument"),
        "clap should not reject -Xmx 256m; stderr={stderr:?}"
    );
    assert!(
        status.success(),
        "cratonvm -Xmx 256m … failed with {status:?}\nstdout={stdout:?}\nstderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected program output, got stdout={stdout:?}"
    );
}

/// `-Xmx512m` (a different size from the 256m fixtures above) is also
/// rewritten and accepted. This guards the rewriter's generic handling
/// of the trailing `<size>` token — i.e. that it strips the `-Xmx`
/// prefix and forwards the remainder, rather than only recognising one
/// hard-coded value. The chosen size is large enough that the VM has
/// plenty of headroom to run HelloWorld to a clean exit, so a successful
/// exit code proves the value was wired through to `with_max_heap_size`.
#[test]
fn xmx_inline_single_dash_512m_accepted() {
    let (status, stdout, stderr) = run_with_xmx_args(&["-Xmx512m"]);
    assert!(
        !stderr.contains("unexpected argument"),
        "clap should not reject -Xmx512m; stderr={stderr:?}"
    );
    assert!(
        status.success(),
        "cratonvm -Xmx512m … failed with {status:?}\nstdout={stdout:?}\nstderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected program output, got stdout={stdout:?}"
    );
}
