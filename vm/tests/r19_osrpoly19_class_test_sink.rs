// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 wave 1 (lane osrpoly19; proposals OSRPOLY18-1 / OSRFOLLOW18-3
//! of `docs/feature-designs/deopt-osr.md` and
//! `docs/feature-designs/deopt-osr.md`): the optimizing tier's polymorphic
//! guarded splice with its class tests sunk to their branches
//! (`CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH`), fused
//! (`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`), and scoped to OSR-door compiles
//! (`CRATONVM_JIT_OSR_DOOR_IR_POLYMORPHIC_SPLICE`) or process-wide
//! (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`). Every arm, in both modes,
//! must print HotSpot's output, the null receiver's NPE included.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

const SWITCHES: [&str; 4] = [
    "CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH",
    "CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH",
    "CRATONVM_JIT_OSR_DOOR_IR_POLYMORPHIC_SPLICE",
    "CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE",
];

#[test]
fn polymorphic_class_test_chain_arms_match_hotspot() {
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
        .join("tests/r19_osrpoly19_class_test_sink_fixtures/OsrPolyShapesProbe.java");
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
        .arg("OsrPolyShapesProbe")
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains("OSR-POLY-SHAPES-OK"));
    // Each arm names the switches it turns on; every other one is removed.
    let arms: [&[&str]; 4] = [
        &[],
        &[SWITCHES[0], SWITCHES[1], SWITCHES[2]],
        &[SWITCHES[0], SWITCHES[3]],
        &[SWITCHES[2]],
    ];
    for mode in ["--jdk-only", "--compatible"] {
        for arm in arms {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .arg("-cp")
                .arg(dir.path())
                .arg("OsrPolyShapesProbe")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for key in SWITCHES {
                if arm.contains(&key) {
                    cmd.env(key, "1");
                } else {
                    cmd.env_remove(key);
                }
            }
            let done = common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
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
