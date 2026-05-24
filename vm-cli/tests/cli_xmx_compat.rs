//! HotSpot `-Xmx` single-dash compatibility (review item C36 / "HIGH bug"
//! in `.claude/review-2026-05-24/vm-cli.md` §1).
//!
//! Stock `java -Xmx256m HelloWorld` (and the two-token form
//! `java -Xmx 256m HelloWorld`) MUST be accepted by the `java` bin alias
//! the cratonvm-cli ships — that is the whole reason the alias exists
//! (Maven Surefire / Gradle / IDE launchers all pass `-Xmx`). Today the
//! clap attribute `#[arg(long = "Xmx")]` parses only the double-dash
//! spelling `--Xmx`, so single-dash invocations fail at the clap layer
//! with exit code 2 and `error: unexpected argument '-Xmx256m' found`.
//!
//! Both tests below are `#[ignore]` because they assert the **desired**
//! post-fix behaviour. Once a pre-clap rewriter (analogous to
//! `normalize_java_launcher_argv` for `-jar`/`-cp`) lands and folds
//! `-Xmx` / `-Xmx 256m` into clap's understood form, drop the `#[ignore]`
//! and the tests start guarding the fix from regression.
//!
//! Run locally with `cargo test --test cli_xmx_compat -- --ignored`.

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

#[test]
#[ignore = "C36/HIGH: pre-clap rewriter for `-Xmx<size>` not yet implemented"]
fn xmx_inline_single_dash_accepted() {
    let (status, stdout, stderr) = run_with_xmx_args(&["-Xmx256m"]);
    assert!(
        status.success(),
        "cratonvm -Xmx256m … failed with {status:?}\nstdout={stdout:?}\nstderr={stderr:?}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "clap should not reject -Xmx256m; stderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected program output, got stdout={stdout:?}"
    );
}

#[test]
#[ignore = "C36/HIGH: pre-clap rewriter for `-Xmx <size>` (two-token) not yet implemented"]
fn xmx_separate_token_single_dash_accepted() {
    let (status, stdout, stderr) = run_with_xmx_args(&["-Xmx", "256m"]);
    assert!(
        status.success(),
        "cratonvm -Xmx 256m … failed with {status:?}\nstdout={stdout:?}\nstderr={stderr:?}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "clap should not reject -Xmx 256m; stderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected program output, got stdout={stdout:?}"
    );
}

/// Regression-marker (not `#[ignore]`d): asserts the CURRENT buggy
/// behaviour so the day the fix lands, this test starts failing and
/// reminds whoever did it to flip the two assertions above off
/// `#[ignore]`. Mirrors the enhancement suggested in
/// `.claude/review-2026-05-24/vm-cli.md` §2 "Enhancements" line 112.
#[test]
fn xmx_single_dash_currently_rejected_by_clap() {
    let (status, _stdout, stderr) = run_with_xmx_args(&["-Xmx256m"]);
    if status.success() {
        // The bug has been fixed; surface a clear message so the dev
        // remembers to remove the `#[ignore]` on the two tests above.
        panic!(
            "C36 fix appears to have landed (-Xmx256m now accepted). \
             Remove the #[ignore] from `xmx_inline_single_dash_accepted` \
             and `xmx_separate_token_single_dash_accepted`, then delete \
             this regression-marker test."
        );
    }
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("-Xmx"),
        "current bug: expected clap to reject `-Xmx256m`; stderr={stderr:?}"
    );
}
