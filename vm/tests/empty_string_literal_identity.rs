// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK 25 returns the pooled `""` literal for an empty builder's `toString()`
//! (`length() == 0 ? "" : new String(this, null)`) and for an empty
//! `String.repeat` (`if (len == 0 || count == 0) return "";`), so
//! `new StringBuilder().toString() == ""` is `true` on HotSpot. The
//! interpreter's natives for both answered a FRESH empty String, so the
//! identity read `false` wherever the interpreter's inline cache served the
//! call -- every call under `--nojit`, the warm-up calls otherwise (strings
//! round 2, `str17`). Both modes, with and without the JIT.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn an_empty_builder_and_an_empty_repeat_are_the_pooled_literal_in_both_modes() {
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let binary = std::env::var_os("CRATONVM_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            ["release", "debug"]
                .into_iter()
                .map(|p| target.join(p).join(exe))
                .find(|p| p.is_file())
        });
    let Some(binary) = common::require_binary(binary.filter(|p| p.is_file())) else {
        return;
    };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .find(|p| {
            p.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
                .is_file()
        });
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/empty_string_literal_identity_fixtures/EmptyStringLiteralProbe.java");
    let compiled = match Command::new(
        home.join("bin")
            .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
    )
    .args(["-d"])
    .arg(dir.path())
    .arg(source)
    .output()
    {
        Ok(out) => out,
        // javac cannot be launched at all -- the one legitimate skip.
        Err(e) => {
            eprintln!("[empty_string_literal_identity] javac could not be executed: {e}; skipping");
            return;
        }
    };
    assert!(
        compiled.status.success(),
        "[empty_string_literal_identity] EmptyStringLiteralProbe.java failed to compile -- fix \
         the probe source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let expected = "sb=50000 sb16=50000 setLength=50000 repeat0=50000 repeatEmpty=50000 \
                    repeat1=50000 of 50000";
    for mode in ["--jdk-only", "--compatible"] {
        for nojit in [false, true] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode);
            if nojit {
                cmd.arg("--nojit");
            }
            cmd.arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("EmptyStringLiteralProbe")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done =
                common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(180));
            assert!(!done.timed_out, "probe timed out: {mode}, nojit={nojit}");
            let stdout = String::from_utf8_lossy(&done.output.stdout);
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                done.output.status.success()
                    && stdout.contains(expected)
                    && stdout.contains("fresh=true equal=true"),
                "{mode}, nojit={nojit}: expected `{expected}` and `fresh=true equal=true`\n\
                 {stdout}\n{stderr}"
            );
        }
    }
}
