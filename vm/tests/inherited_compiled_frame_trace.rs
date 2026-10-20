// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A compiled INHERITED method must report its declaring class in a stack
//! trace, so a trace does not change once the method compiles.
//!
//! Drives the checked-in probe `tools/probes/InheritedCompiledFrameTrace.java`,
//! which repeats two throw sites and prints, per row, `stable true|false` (every
//! repetition's trace equals the first) and the first and last traces with
//! file names and lines:
//!
//! * `user`: `Sub` inherits `make` from `Base`; the exception is built in
//!   `Base.make`, reached through a `Sub` receiver;
//! * `platform`: a delegation miss through the platform class loader, whose
//!   top frames are the JDK's `BuiltinClassLoader.loadClass` and
//!   `ClassLoader.loadClass`, reached through a `ClassLoaders$PlatformClassLoader`
//!   receiver.
//!
//! # The defect this pins
//!
//! The by-name callee door (`try_jit_compile_callee_slow`) publishes an
//! inherited method's body under its RECEIVER's key, and `JitCache::put`
//! stamped that receiver class as the body's owner, which every live-frame
//! reader took as the frame's class. The frame then named the method from the
//! label but read its source file and line from the subclass:
//!
//! ```text
//! user last      HotSpot : Base.make(InheritedCompiledFrameTrace.java:31)
//!                CratonVM: Base.make(InheritedCompiledFrameTrace.java:-1)
//! platform last  HotSpot : BuiltinClassLoader.loadClass(BuiltinClassLoader.java:580)
//!                CratonVM: BuiltinClassLoader.loadClass(ClassLoaders.java:-1)
//! ```
//!
//! First seen as `L2W73ThrowableTraceShapes two-sites-stable false` under
//! `--jdk-only` (`docs/internal/jit-proposals/i74-L2-l2w73-throwable-trace-shapes-jdk-only-diff-FIXED-20261011.md`).
//!
//! # The oracle is the interpreter
//!
//! `CRATONVM_DISABLE_JIT=1` on the same binary and the same classes must print
//! the same stdout as the default arm, in both compatibility modes. The
//! interpreter's frames carry their own class, so they never had the defect.
//!
//! # Anti-vacuity
//!
//! If nothing compiled, the default arm equals the oracle trivially. So the
//! kill switch `CRATONVM_JIT_FRAME_REPORTS_HOLDER_CLASS=0` must DIFFER from the
//! oracle in at least one of the two modes. Not in each: both modes publish
//! `Sub.make` under the receiver key, but whether a throw then runs that body
//! or `Base`'s own is a dispatch race (measured 2026-10-11: `--jdk-only`
//! reverted in every run, `--compatible` in 10 of 14). A run where
//! neither mode reverts is an environment outcome: loud on stderr, a failure
//! under `CRATONVM_REQUIRE_E2E`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

/// Tag on every diagnostic this file emits.
const TAG: &str = "inherited_compiled_frame_trace";

/// The probe's class name.
const PROBE: &str = "InheritedCompiledFrameTrace";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

/// The checked-in probe; only the tracked `tools/probes/` home is searched
/// (see `stack_trace_across_tiers.rs` for why).
fn probe_source() -> Option<PathBuf> {
    common::require_fixture(
        TAG,
        "the InheritedCompiledFrameTrace probe (tools/probes/InheritedCompiledFrameTrace.java)",
        &[workspace_root()
            .join("tools")
            .join("probes")
            .join("InheritedCompiledFrameTrace.java")],
    )
}

/// Per-test-binary class output, so no other harness can race our `javac`.
fn probe_classes_dir() -> PathBuf {
    workspace_root().join("target").join(TAG)
}

fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let target = workspace_root().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for cand in [
        "/home/victor/jdk25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// `-g` is not optional: without a LineNumberTable every line is `-1` in both
/// arms and the comparison is vacuous.
fn compile_probe(javac: &Path, src: &Path) -> Option<PathBuf> {
    let dir = probe_classes_dir();
    let _ = std::fs::create_dir_all(&dir);
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let class_file = dir.join(format!("{PROBE}.class"));
    let _ = std::fs::remove_file(&class_file);
    let out = match Command::new(javac)
        .args(["-g", "--release", "21", "-d"])
        .arg(&dir)
        .arg(src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[{TAG}] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_text = String::from_utf8_lossy(&out.stderr);
        if stderr_text.contains("release version") && stderr_text.contains("not supported") {
            eprintln!(
                "[{TAG}] javac cannot target --release 21 ({}); skipping. Point JAVA_HOME or \
                 CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_text.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && class_file.exists(),
        "[{TAG}] the probe failed to compile — fix tools/probes/InheritedCompiledFrameTrace.java. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// Run one arm; returns stdout. Drained through `common::wait_draining`.
fn run_arm(
    bin: &Path,
    jdk: &Path,
    classes: &Path,
    arm: &str,
    mode_flag: Option<&str>,
    extra_env: &[(&str, &str)],
) -> String {
    let mut cmd = Command::new(bin);
    if let Some(flag) = mode_flag {
        cmd.arg(flag);
    }
    cmd.arg("--java-home").arg(jdk);
    for &(key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("-c")
        .arg(classes)
        .arg(PROBE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("[{TAG}] {arm}: could not spawn cratonvm: {e}"));
    let timed = common::wait_draining(child, Duration::from_secs(600));
    let out = timed.output;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !timed.timed_out,
        "[{TAG}] {arm}: the probe did not finish within the cap. stdout:\n{stdout}"
    );
    assert!(
        out.status.success(),
        "[{TAG}] {arm}: the probe exited with {:?}. stdout:\n{stdout}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The probe's six report lines, in order, or a panic naming what is missing.
fn rows(stdout: &str, arm: &str) -> Vec<String> {
    let want = [
        "user stable ",
        "user first ",
        "user last ",
        "platform stable ",
        "platform first ",
        "platform last ",
    ];
    let lines: Vec<&str> = stdout.lines().collect();
    want.iter()
        .map(|prefix| {
            lines
                .iter()
                .find(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("[{TAG}] {arm}: no `{prefix}` line in:\n{stdout}"))
                .to_string()
        })
        .collect()
}

#[test]
fn a_compiled_inherited_method_reports_its_declaring_class() {
    let Some(src) = probe_source() else { return };
    let Some(bin) = cratonvm_binary() else { return };
    let Some(jdk) = jdk_home() else { return };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let Some(classes) = compile_probe(&javac, &src) else {
        return;
    };

    let mut reverted_in = Vec::new();
    for mode in [None, Some("--compatible")] {
        let label = mode.unwrap_or("--jdk-only");
        let oracle = rows(
            &run_arm(
                &bin,
                &jdk,
                &classes,
                "interpreter",
                mode,
                &[("CRATONVM_DISABLE_JIT", "1")],
            ),
            "interpreter",
        );
        // The oracle itself: one throw site, so every repetition is equal and
        // every frame has a line.
        assert_eq!(
            oracle[0], "user stable true",
            "[{TAG}] {label}: interpreter oracle"
        );
        assert_eq!(
            oracle[3], "platform stable true",
            "[{TAG}] {label}: interpreter oracle"
        );
        assert!(
            !oracle
                .iter()
                .any(|r| r.contains(":-1)") || r.contains("(null:")),
            "[{TAG}] {label}: the interpreter oracle has a frame with no line or file: {oracle:#?}"
        );

        let default = rows(
            &run_arm(&bin, &jdk, &classes, "default", mode, &[]),
            "default",
        );
        assert_eq!(
            default, oracle,
            "[{TAG}] {label}: a compiled frame changed the trace (left: JIT on, right: \
             CRATONVM_DISABLE_JIT=1)"
        );

        let reverted = rows(
            &run_arm(
                &bin,
                &jdk,
                &classes,
                "kill-switch",
                mode,
                &[("CRATONVM_JIT_FRAME_REPORTS_HOLDER_CLASS", "0")],
            ),
            "kill-switch",
        );
        if reverted == oracle {
            eprintln!(
                "[{TAG}] {label}: note: CRATONVM_JIT_FRAME_REPORTS_HOLDER_CLASS=0 printed the \
                 oracle's trace (no receiver-keyed inherited body ran a throw in this mode)"
            );
        } else {
            reverted_in.push(label);
        }
    }
    if reverted_in.is_empty() {
        let msg = format!(
            "[{TAG}] ENVIRONMENT: CRATONVM_JIT_FRAME_REPORTS_HOLDER_CLASS=0 printed the oracle's \
             trace in both modes, so no inherited body compiled under its receiver's key ran a \
             throw and the default arms' agreement proves nothing"
        );
        eprintln!("{msg}");
        assert!(!common::require_e2e(), "{msg}");
    }
}
