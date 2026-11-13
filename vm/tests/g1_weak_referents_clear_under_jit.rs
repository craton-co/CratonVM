// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation-driven G1 young pauses clear a `WeakReference` whose referent is
//! otherwise unreachable, with the JIT on and a compiled frame holding the
//! references, in both modes, as HotSpot G1 does (`PROBE-OK`, ~177k
//! allocations at `-Xmx256m`).
//!
//! A blocked peer's stale native-stack word pins the OBJECT it names (g1pin),
//! and the pause roots that object by assumption. The reference round then
//! read the pause's identity map entry as "survived" and never offered the
//! referent to the pinned-referent verdict, so both probes printed
//! `inline=LIVE viaMethod=LIVE allocations=2500000 PROBE-FAIL`. `-Xmx256m` is
//! required: on a big heap neither CratonVM nor HotSpot ZGC collects at all in
//! 2.5 M allocations. See `docs/feature-designs/generative-gc-architecture.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

#[test]
fn g1_clears_weak_referents_under_the_jit_at_256m() {
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
    let javac = if cfg!(windows) { "javac.exe" } else { "javac" };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
        .find(|p| p.join("bin").join(javac).is_file());
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let probes = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tools/probes");
    let compiled = Command::new(home.join("bin").join(javac))
        .arg("-d")
        .arg(dir.path())
        .arg(probes.join("WeakClearYoungProbe.java"))
        .arg(probes.join("WeakClearNoLoadProbe.java"))
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "the probes failed to compile. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["--real-jdk", "--compatible"] {
        for probe in ["WeakClearYoungProbe", "WeakClearNoLoadProbe"] {
            let mut cmd = Command::new(&binary);
            cmd.arg(mode)
                .arg("--java-home")
                .arg(&home)
                .args(["-XX:+UseG1GC", "-Xmx256m"])
                .arg("-cp")
                .arg(dir.path())
                .arg(probe)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let done =
                common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(600));
            assert!(!done.timed_out, "probe timed out: {mode} {probe}");
            let stdout = String::from_utf8_lossy(&done.output.stdout);
            assert!(
                stdout.starts_with("inline=cleared viaMethod=cleared allocations=")
                    && stdout.trim_end().ends_with("PROBE-OK"),
                "{mode} {probe}: HotSpot G1 prints `inline=cleared viaMethod=cleared ... PROBE-OK`, got:\n{stdout}\n{}",
                String::from_utf8_lossy(&done.output.stderr)
            );
        }
    }
}
