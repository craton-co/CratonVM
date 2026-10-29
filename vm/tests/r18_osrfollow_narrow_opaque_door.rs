// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 11 (lane osrfollow): the optimizing OSR door under the
//! narrowed opaque policy (`CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR`, OSRPOLY18-2),
//! with and without the round-13 handler-header arms, and the fused class-test
//! branch with the IR polymorphic splice (`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`,
//! `CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`, OSRPOLY18-1). Every arm, in both
//! modes, must print HotSpot's output: loops with exact guards, a pruned arm
//! that fires after the OSR entry, catches that continue the loop (one
//! rewriting a local the loop reads), and polymorphic interface sites.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

const SWITCHES: [&str; 5] = [
    "CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR",
    "CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS",
    "CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH",
    "CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE",
    "CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_TOKENS",
];

#[test]
fn narrowed_osr_door_and_fused_class_tests_match_hotspot() {
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
        .join("tests/r18_osrfollow_narrow_opaque_door_fixtures/OsrFollowProbe.java");
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
        .arg("OsrFollowProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("OSR-FOLLOW-OK"));
    let arms: [&[(&str, &str)]; 5] = [
        &[],
        &[("CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR", "1")],
        &[
            ("CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR", "1"),
            ("CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS", "0"),
        ],
        &[("CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH", "1")],
        &[
            ("CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR", "1"),
            ("CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH", "1"),
            ("CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE", "1"),
            ("CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_TOKENS", "1"),
        ],
    ];
    for mode in ["--jdk-only", "--compatible"] {
        for arm in arms {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("OsrFollowProbe")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for key in SWITCHES {
                cmd.env_remove(key);
            }
            for &(key, word) in arm {
                cmd.env(key, word);
            }
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(180));
            let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
            let stderr = String::from_utf8_lossy(&done.output.stderr);
            assert!(
                !done.timed_out && done.output.status.success(),
                "{mode}/{arm:?}: {stdout}\n{stderr}"
            );
            assert_eq!(stdout, expected, "{mode}/{arm:?}: {stderr}");
        }
    }
}
