// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Every access mode of a real `int[]` / `long[]` element `VarHandle`, hot
//! enough to be compiled, prints HotSpot's output in both modes -- with the
//! in-funnel fast path on and with it switched off -- and the fast path really
//! serves the accesses.
//!
//! `AtomicIntegerArray.get` cost 1.2-1.6 us through the generic VarHandle
//! native (0.7 ns on HotSpot), and netty's `MpscIntQueue` -- the free list of
//! every `AdaptivePoolingAllocator` size class -- is made of them, so an
//! alloc / grow / release cycle cost 17 us against 0.1 us and
//! `AdaptiveByteBufAllocatorGrowthTest` ran 170-300 s against a 180 s cap. See
//! `docs/internal/fixed-suite-bugs/netty/adaptivebytebufallocator-searchprocessor-per-call-throughput-wall-FIXED-20261012.md`.
//!
//! The probe also drives the shapes the fast path must decline (a widening
//! site, an `Object` site, a null array, an index out of range) and two
//! threads racing on the same elements, so a served access that lost an update
//! or swallowed an exception would show in the output.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str =
    include_str!("varhandle_array_element_fast_fixtures/VarHandleArrayElementProbe.expected");

fn served(stderr: &str) -> Option<u64> {
    stderr.lines().find_map(|l| {
        l.split("VarHandle int[]/long[] element accesses served directly: ")
            .nth(1)
            .and_then(|n| n.trim().parse().ok())
    })
}

#[test]
fn array_element_var_handle_accesses_match_hotspot_and_are_served_directly() {
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
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
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
        .join("tests/varhandle_array_element_fast_fixtures/VarHandleArrayElementProbe.java");
    let compiled =
        Command::new(
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
        )
        .args(["-d"])
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    // (mode, in-funnel fast path on, `jdk-atomic-arrays` row on): the row only
    // exists under `--compatible`, where it decides whether `AtomicIntegerArray`
    // and `AtomicLongArray` run the JDK's bodies or the VM's stand-ins.
    let configs = [
        ("--jdk-only", true, true),
        ("--jdk-only", false, true),
        ("--compatible", true, true),
        ("--compatible", false, true),
        ("--compatible", true, false),
    ];
    for (mode, fast, row) in configs {
        let mut cmd = Command::new(&binary);
        cmd.arg(mode)
            .arg("--java-home")
            .arg(&home)
            .arg("-cp")
            .arg(dir.path())
            .arg("VarHandleArrayElementProbe")
            .env("CRATONVM_DBG", "intrinsic-stats")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !fast {
            cmd.env("CRATONVM_JIT", "-varhandle-array-element-fast");
        }
        if !row {
            cmd.env("CRATONVM_COMPAT_ALIGN", "-jdk-atomic-arrays");
        }
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(300));
        assert!(
            !done.timed_out,
            "probe timed out: {mode} fast={fast} row={row}"
        );
        let stdout = String::from_utf8_lossy(&done.output.stdout);
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            done.output.status.success() && stdout == EXPECTED,
            "{mode} fast={fast} row={row}: stdout differs from HotSpot's:
{stdout}
{stderr}"
        );
        if let Some(n) = served(&stderr) {
            if fast && mode == "--jdk-only" {
                assert!(
                    n > 1000,
                    "{mode}: the array-element accesses were not served in the funnel ({n}):
{stderr}"
                );
            }
            if !fast {
                assert_eq!(
                    n, 0,
                    "{mode}: the kill switch left the fast path on:
{stderr}"
                );
            }
        }
    }
}
