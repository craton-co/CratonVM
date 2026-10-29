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
//! `docs/feature-designs/jdk-only-mode.md`.
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
    // (mode, in-funnel fast path on, `jdk-atomic-arrays` row on, compiled
    // constant-handle array-element arm on): the row only exists under
    // `--compatible`, where it decides whether `AtomicIntegerArray` and
    // `AtomicLongArray` run the JDK's bodies or the VM's stand-ins.
    //
    // JIT round 22 wave 7, lane sync: the fourth column. Since JIT r20 wave 18
    // (lane xVhArr, `CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT`, default ON) a
    // compiled `static final` array-element `VarHandle` site -- every exact site
    // of this probe, `AtomicIntegerArray.AA` / `AtomicLongArray.AA` and the
    // probe's own `INTS` / `LONGS` -- is served INLINE before its call, so it
    // never reaches the dispatch funnel this test's `served` count measures
    // (on the w6 build and on dev: `served directly: 0`, and the ~285k funnel
    // calls are the probe's declined-by-design widening / boxing / `Object`
    // sites). The funnel fast path is therefore measured with that arm off, and
    // the default configuration still checks HotSpot's output.
    let configs = [
        ("--jdk-only", true, true, true),
        ("--jdk-only", true, true, false),
        ("--jdk-only", false, true, false),
        ("--compatible", true, true, true),
        ("--compatible", false, true, true),
        ("--compatible", true, false, true),
    ];
    for (mode, fast, row, inline_arm) in configs {
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
        if !inline_arm {
            cmd.env("CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT", "0");
        }
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(300));
        assert!(
            !done.timed_out,
            "probe timed out: {mode} fast={fast} row={row} inline_arm={inline_arm}"
        );
        // JIT round 22 wave 7, lane sync: `line.separator` is `\r\n` on Windows,
        // as on HotSpot there (`native-builtins` `lib.rs`), and the fixture is
        // stored with `\n`: compare line contents, as the sibling probe tests do
        // (`jit_atomic_integer_access.rs`). This was the whole stdout "mismatch"
        // on Windows, on dev too: the lines were HotSpot's.
        let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            done.output.status.success() && stdout == EXPECTED,
            "{mode} fast={fast} row={row} inline_arm={inline_arm}: stdout differs from HotSpot's:
{stdout}
{stderr}"
        );
        if let Some(n) = served(&stderr) {
            if fast && mode == "--jdk-only" && !inline_arm {
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
