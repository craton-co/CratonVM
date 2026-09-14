// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test for the `ArrayList$Itr` `lastRet`/`cursor` field-slot
//! collision bug (found while investigating a reported infinite loop
//! loading `org.wildfly.extension.core-management`, a `java.desktop`-
//! dependent WildFly module — see `docs/known-issues/`).
//!
//! `al_itr_last_ret_slot` (`native-collections/src/lib.rs`) resolves the
//! real `ArrayList$Itr.lastRet` field by name and falls back to a fixed
//! slot index only when that real class isn't available — i.e. when
//! CratonVM's own synthetic `java/util/ArrayList$Itr` layout is in use
//! (the common case; the real class is only present when actual
//! `AbstractList` bytecode itself constructed the `Itr`). That fallback
//! defaulted to slot 1 — the exact same slot as `AL_ITR_FIELD_CURSOR`.
//! `native_al_itr_next`'s last line, `lastRet = cursor` (using the
//! *pre-increment* cursor value), then unconditionally overwrote the
//! `cursor = cursor + 1` write one line above it, so `cursor` never
//! advanced past 0 — `hasNext()` (`cursor < size`) stayed `true` forever
//! and `next()` returned the same first element on every call.
//!
//! A Java-level loop draining *any* ArrayList iterator this way never
//! terminates. This test spawns the real `cratonvm` binary as a
//! subprocess — not an in-process `Vm::invoke` — specifically so a
//! regression (a genuine infinite loop, not a slow computation) gets
//! killed by the timeout instead of hanging the whole test run.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests").join("al_itr_lastret_probe_fixtures")
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
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
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

/// Compile `AlItrLastRetProbe.java` via `javac` if the checked-in `.class`
/// is missing. The `.class` is committed alongside the source (matching
/// `vm/tests/wildfly_boot_fixtures`'s convention) so this test still runs
/// without `javac` on `PATH`.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("AlItrLastRetProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("AlItrLastRetProbe.java");
    if !source.exists() {
        return false;
    }
    let _ = std::fs::create_dir_all(&dir);
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: answering `false` here reads to
        // the caller as "javac unavailable, skip", which makes this test a
        // permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[al_itr_lastret_slot_regression] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[al_itr_lastret] the checked-in probe fixture failed to compile — fix the \
                 .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// Runs `AlItrLastRetProbe` under a hard timeout, killing the subprocess
/// if exceeded — a regression here is a genuine infinite loop.
fn run_probe(timeout: Duration) -> Option<(String, String)> {
    if !ensure_probe_compiled() {
        eprintln!(
            "[al_itr_lastret_slot_regression] AlItrLastRetProbe.class unavailable \
             (javac on PATH?); skipping"
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[al_itr_lastret_slot_regression] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let dir = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&dir)
        .arg("AlItrLastRetProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[al_itr_lastret_slot_regression] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[al_itr_lastret_slot_regression] AlItrLastRetProbe timed out \
                         after {timeout:?} — likely a regression of the \
                         ArrayList$Itr lastRet/cursor slot-collision bug \
                         (hasNext() never terminates)"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[al_itr_lastret_slot_regression] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[al_itr_lastret_slot_regression] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Draining a one-element `ArrayList`'s iterator must visit exactly one
/// element (not spin forever re-visiting the first), and `Iterator.remove()`
/// must still work correctly with the `lastRet` field at its new slot.
/// Both must complete within 15s (generous; the real workload finishes in
/// well under a second).
#[test]
fn arraylist_iterator_terminates_and_remove_still_works() {
    let (stdout, stderr) = match run_probe(Duration::from_secs(15)) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("single_element_seen=1"),
        "iterating a 1-element ArrayList should visit exactly 1 element \
         (a regression re-visits the same element forever) — got:\n{combined}"
    );
    assert!(
        combined.contains("visited=abcd"),
        "iterating a 4-element ArrayList should visit each element exactly \
         once, in order — got:\n{combined}"
    );
    assert!(
        combined.contains("remaining=[a, c]"),
        "Iterator.remove() on 'b' and 'd' should leave [a, c] — got:\n{combined}"
    );
    assert!(
        combined.contains("DONE"),
        "probe did not reach its final marker — got:\n{combined}"
    );
}
