// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for `build_set`'s cross-call GC-safety fix
//! (`native-io/src/nio_selector.rs`, 2026-07-07). The fixture drives many
//! `Selector.selectedKeys()`/`.keys()` cycles under `CRATONVM_GC_STRESS` so
//! the native `HashSet` construction (`HashSet.<init>` + repeated `Set.add`,
//! each capable of triggering a moving GC) is exercised many times per run —
//! before the fix, this reproduced as an all-zero-header `java/util/Set`
//! receiver + `NoSuchMethodError Object.add`, matching the Tomcat Tribes
//! `NioReceiver.listen()` crash shape found via the
//! `nio-native-side-table-stale-objectref` doc audit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "NioSelectorBuildSetGc";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
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
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = workspace_root().join("target").join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn classpath_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("resources"),
    );
    candidates.into_iter().find(|dir| {
        dir.join("cratonvm")
            .join(format!("{FIXTURE}.class"))
            .exists()
    })
}

// Un-ignored 2026-08-04. The `#[ignore]` this carried pointed at
// `serversocket-bind-null-inetaddress-net-sockets-20260803`: under
// `CRATONVM_REAL=net-sockets` + GC stress, `new ServerSocket(0)` (line 28 of
// the fixture) NPE'd inside `sun.nio.ch.Net.bind` because the wildcard
// `InetAddress` arrived null. Root cause was two cross-call GC-safety defects
// — the `InetSocketAddress`/`InetAddress` construction path in native-builtins
// losing its own freshly-allocated objects across a cold class load, and
// `Class.getEnumConstants()` copying out of a relocated `$VALUES`. Both are
// fixed; the assertions below are unchanged from when they were written.
#[test]
fn nio_selector_selected_keys_survives_gc_stress() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[nio_selector_build_set_gc] cratonvm binary not found; skipping");
        return;
    };
    let Some(classpath) = classpath_dir() else {
        eprintln!("[nio_selector_build_set_gc] {FIXTURE}.class not compiled; skipping");
        return;
    };

    let mut child = Command::new(&bin)
        .arg("--nojit")
        .arg("--Xmx")
        .arg("128m")
        .arg("-c")
        .arg(&classpath)
        .arg(format!("cratonvm.{FIXTURE}"))
        // Grouped spelling. The per-flag `CRATONVM_DISABLE_DEFAULT_WATCHDOG` /
        // `CRATONVM_REAL_NET_SOCKETS` / `CRATONVM_GC_STRESS` /
        // `CRATONVM_MOVING_YOUNG` variables are REJECTED at startup now — the
        // launcher prints the supported spelling and refuses to boot, so this
        // probe was measuring a VM that never started rather than a selector
        // under GC stress.
        //
        // `stress` carries its magnitude in the grouped value; the two GC
        // tokens go in ONE `CRATONVM_GC`, because a second assignment replaces
        // the first rather than adding to it.
        .env("CRATONVM_THREADS", "-default-watchdog")
        .env("CRATONVM_REAL", "net-sockets")
        .env("CRATONVM_GC", "stress=65536,moving-young")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("[nio_selector_build_set_gc] failed to spawn {bin:?}: {e}"));

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > RUN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[nio_selector_build_set_gc] {FIXTURE} timed out after {RUN_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[nio_selector_build_set_gc] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("[nio_selector_build_set_gc] wait_with_output failed");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert!(
        output.status.success() && stdout.contains("NIO_SELECTOR_BUILD_SET_GC_OK"),
        "{FIXTURE} failed under Selector.selectedKeys() GC stress.\nstatus={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout,
        stderr
    );
    assert!(
        !stderr.contains("Stale pointer detected") && !stderr.contains("NoSuchMethodError"),
        "{FIXTURE} exited OK but stale-receiver/NoSuchMethodError warnings were logged \
         (regression signal even without a hard failure).\nstderr:\n{}",
        stderr
    );
}
