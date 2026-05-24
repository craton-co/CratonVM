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
//! Bonus: this is the only e2e coverage that touches the MED bug at
//! `vm-cli/src/main.rs:1392` — `string_array_class_id = ClassId::new(0)`
//! is hard-coded to `java/lang/Object`, but `main(String[])` is
//! supposed to receive a `[Ljava/lang/String;`. If a future fix
//! resolves the actual `String` ClassId, PrintArgs's `args[i].length()`
//! style downstream uses (not exercised here, but a follow-up test
//! could add `args[i].length()` and check the printed integer)
//! continue working — this test just guards the argv plumbing itself.

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
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    assert!(
        lines.windows(3).any(|w| w == ["alpha", "beta", "gamma"]),
        "expected alpha/beta/gamma to appear in order on stdout; got lines={lines:?}"
    );
}
