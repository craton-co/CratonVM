// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Empty builders return the pooled literal in interpretation. HotSpot C2's
//! OptimizeStringConcat instead returns a fresh empty String for fused chains.
//! Exercise both compiled policies and the interpreter in both VM modes;
//! String.repeat and non-fused setLength keep their literal identities.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn empty_strings_preserve_contents_and_the_selected_tier_identity_policy() {
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
    let compiled =
        match Command::new(
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
                eprintln!(
                    "[empty_string_literal_identity] javac could not be executed: {e}; skipping"
                );
                return;
            }
        };
    assert!(
        compiled.status.success(),
        "[empty_string_literal_identity] EmptyStringLiteralProbe.java failed to compile -- fix \
         the probe source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let expected = "setLength=50000 repeat0=50000 repeatEmpty=50000 repeat1=50000 of 50000";
    for mode in ["--real-jdk", "--compatible"] {
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
                    && stdout.contains("fresh=true equal=true")
                    && stdout.contains("empty=true"),
                "{mode}, nojit={nojit}: expected `{expected}`, `empty=true` and `fresh=true equal=true`\n\
                 {stdout}\n{stderr}"
            );
            let count = |prefix: &str| {
                stdout
                    .split_whitespace()
                    .find_map(|word| word.strip_prefix(prefix))
                    .and_then(|n| n.parse::<u32>().ok())
                    .expect("probe reports an identity count")
            };
            for prefix in ["sb=", "sb16="] {
                let identities = count(prefix);
                if nojit {
                    assert_eq!(identities, 50_000, "{mode}, nojit={nojit}, {prefix}");
                } else {
                    assert!(
                        identities < 50_000,
                        "{mode}, {prefix}: no fused fresh result observed"
                    );
                }
            }
        }
    }
}
