// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A compiled caller must not keep a reference argument alive once the callee
//! it hands it to has dropped it (HotSpot's outgoing arguments belong to the
//! callee).
//!
//! `tools/bench/Gcd1ArgPinSpecialProbe more settle` runs six shapes; the
//! `virtual-direct` and `interface-direct` ones failed in `--compatible` G1
//! because the inline-cache hit's callee deopted, and the call-site service
//! (`jit_service_callee_deopt` -> `try_resume_trapped_callee`) resumed it in
//! the interpreter while the caller's argument block stayed named in the
//! caller's oop map for the whole resumed run. HotSpot 25 prints `PASS all 6`.
//!
//! Page: `docs/feature-designs/native-dispatch-architecture.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

fn cratonvm_binary() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return common::require_binary(Some(p));
        }
    }
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("target");
    let exe = if cfg!(windows) { "cratonvm.exe" } else { "cratonvm" };
    let found = ["release", "debug"]
        .iter()
        .map(|profile| target.join(profile).join(exe))
        .find(|candidate| candidate.exists());
    common::require_binary(found)
}

fn jdk_home() -> Option<PathBuf> {
    let found = ["CRATONVM_TEST_JDK", "JAVA_HOME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(PathBuf::from)
        .find(|p| p.exists());
    common::require_jdk(found)
}

#[test]
fn compatible_inline_cache_callers_release_a_dropped_argument() {
    let (Some(bin), Some(jdk)) = (cratonvm_binary(), jdk_home()) else {
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let out = std::env::temp_dir().join("cratonvm-argfix-caller-release");
    let _ = std::fs::create_dir_all(&out);
    let javac = jdk.join(if cfg!(windows) { "bin/javac.exe" } else { "bin/javac" });
    let compiled = Command::new(&javac)
        .arg("-d")
        .arg(&out)
        .arg(root.join("tools/bench/Gcd1ArgPinSpecialProbe.java"))
        .status();
    let Ok(status) = compiled else {
        eprintln!("[argfix_compiled_caller_releases_dropped_argument] no javac; skipping");
        return;
    };
    assert!(status.success(), "the probe failed to compile");
    let mut cmd = Command::new(&bin);
    for flag in [
        "BY_NAME_TAIL_UNPINS_ARGS",
        "INTERP_DOOR_ARGS_PROMISE",
        "IR_CALLEE_OWNED_ARGS",
        "IR_CALLEE_ARG_WINDOW",
        "IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY",
        "SERVICE_COPY_WINDOW",
        "OSR_SERVICE_COPY_WINDOW",
    ] {
        cmd.env(format!("CRATONVM_JIT_{flag}"), "1");
    }
    let child = cmd
        .args(["--compatible", "--java-home"])
        .arg(&jdk)
        .args(["-XX:+UseG1GC", "-Xmx64m", "-cp"])
        .arg(&out)
        .args(["Gcd1ArgPinSpecialProbe", "more", "settle"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cratonvm");
    let done = common::wait_draining(child, Duration::from_secs(300));
    let stdout = String::from_utf8_lossy(&done.output.stdout).into_owned();
    assert!(!done.timed_out, "the probe timed out.\nstdout:\n{stdout}");
    assert!(
        stdout.contains("PASS all 6"),
        "a compiled caller kept a dropped argument reachable.\nstdout:\n{stdout}"
    );
}
