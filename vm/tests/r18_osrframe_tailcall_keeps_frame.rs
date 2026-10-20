// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 9 (lane osrframe;
//! `docs/internal/fixed-bugs/r18-compatible-osr-inlined-native-throw-loses-the-inline-frame-FIXED-20261006.md`):
//! the single-pass sibling tail call no longer JMPs into a helper that takes
//! the VM context (`CRATONVM_JIT_SP_TAILCALL_KEEPS_VM_HELPER_FRAME`, default
//! ON). Under `--compatible`, a compiled `Objects.checkIndex` tail-called the
//! thin `Preconditions.checkIndex` helper, whose throwing arm captured the
//! trace while the frame-chain mirror still named the torn-down frame: its
//! line came out `-1` (HotSpot `Objects.java:365`), and where the dead frame's
//! saved RBP was overwritten, every frame below it -- an OSR body's inlined
//! callee and its bci -- was lost too. Every arm of both modes must print
//! HotSpot's output; the `=0` arm is run as the control and reported, not
//! asserted (whether the dead frame still reads right is up to the helper's
//! own code generation).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

const SWITCH: &str = "CRATONVM_JIT_SP_TAILCALL_KEEPS_VM_HELPER_FRAME";

#[test]
fn a_tail_call_into_a_vm_helper_keeps_the_callers_frame_in_the_trace() {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
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
    let tool = |name: &str| {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
        .find(|p| p.join("bin").join(tool("javac")).is_file());
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/r18_osrframe_tailcall_keeps_frame_fixtures/R18OsrFrameCheckIndex.java");
    let compiled = Command::new(home.join("bin").join(tool("javac")))
        .arg("-d")
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("javac");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let oracle = Command::new(home.join("bin").join(tool("java")))
        .arg("-cp")
        .arg(dir.path())
        .arg("R18OsrFrameCheckIndex")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("java.util.Objects.checkIndex:365 R18OsrFrameCheckIndex.chk:10"));
    for mode in ["--jdk-only", "--compatible"] {
        for arm in [None, Some("0")] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("R18OsrFrameCheckIndex")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match arm {
                Some(word) => cmd.env(SWITCH, word),
                None => cmd.env_remove(SWITCH),
            };
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}/{arm:?}: {stdout}\n{stderr}"
            );
            match arm {
                None => assert_eq!(stdout, expected, "{mode}: {stderr}"),
                Some(_) => {
                    if stdout != expected {
                        eprintln!("{mode}/{SWITCH}=0 (control) differs from HotSpot:\n{stdout}");
                    }
                }
            }
        }
    }
}
