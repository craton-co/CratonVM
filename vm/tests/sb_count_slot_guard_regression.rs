// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test for the `sb_set_count` field-slot bug (found while
//! investigating an infinite loop reported when loading WildFly's
//! `org.wildfly.extension.core-management` module's real `java.desktop`
//! class graph — see `docs/known-issues/`).
//!
//! CratonVM's synthetic StringBuilder/StringBuffer layout is 2 slots
//! (`value: char[]` @0, `count: int` @1 — see `instance_fields(2)` in
//! `classloading/src/class_manager.rs`). `sb_set_count`
//! (`native-builtins/src/lang_string.rs`) unconditionally wrote slot 1 = 0
//! and mirrored the count into slot 2, on the assumption every
//! StringBuilder has the real JDK 9+ 3-slot layout (`value`/`coder`/`count`
//! @0/1/2 — only true when real `AbstractStringBuilder` bytecode itself
//! constructs the object, e.g. during Byte Buddy retransformation). For
//! the universal 2-slot case this stomped the *only* count-bearing slot to
//! 0 on every append/insert/setLength call and silently dropped the
//! slot-2 write (out of bounds), so `StringBuilder.length()` always read
//! back 0 regardless of how much was appended.
//!
//! A Java-level loop keyed on `sb.length()` (the shape real
//! `java.beans`/AWT clinit code uses, and exactly what `SbCountProbe`'s
//! `padded` loop below reproduces) never observed the length increase and
//! spun forever, hammering the VM's out-of-bounds field write guard on
//! every iteration — millions of log lines in seconds pre-fix.
//!
//! This test spawns the real `cratonvm` binary as a subprocess (not an
//! in-process `Vm::invoke`) specifically so a regression of this bug — a
//! genuine infinite loop, not a slow computation — gets killed by the
//! timeout instead of hanging the whole test run.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests").join("sb_count_probe_fixtures")
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

/// Compile `SbCountProbe.java` via `javac` if the checked-in `.class` is
/// missing. The `.class` is committed alongside the source (matching
/// `vm/tests/wildfly_boot_fixtures`'s convention) so this test still runs
/// without `javac` on `PATH`; recompiles only if the source changed.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("SbCountProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("SbCountProbe.java");
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
                        "[sb_count_slot_guard_regression] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[sb_count_slot_guard] the checked-in probe fixture failed to compile — fix the \
                 .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// Runs `SbCountProbe` under a hard timeout, killing (not just failing)
/// the subprocess if it's exceeded — a regression here is a genuine
/// infinite loop, and letting it run unbounded would hang CI rather than
/// just this test.
fn run_probe(timeout: Duration) -> Option<(String, String)> {
    if !ensure_probe_compiled() {
        eprintln!(
            "[sb_count_slot_guard_regression] SbCountProbe.class unavailable \
             (javac on PATH?); skipping"
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[sb_count_slot_guard_regression] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let dir = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&dir)
        .arg("SbCountProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sb_count_slot_guard_regression] failed to spawn cratonvm: {e}");
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
                        "[sb_count_slot_guard_regression] SbCountProbe timed out \
                         after {timeout:?} — likely a regression of the \
                         sb_set_count field-slot bug (StringBuilder.length() \
                         stuck at 0, a length-keyed loop spins forever)"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[sb_count_slot_guard_regression] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[sb_count_slot_guard_regression] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Plain append/length/toString, buffer growth past the default capacity,
/// and the length-keyed growth loop that hung forever pre-fix must all
/// complete within 15s (generous; the real workload finishes in well
/// under a second) and report the correct lengths/contents.
#[test]
fn stringbuilder_count_survives_append_growth_and_length_loop() {
    let (stdout, stderr) = match run_probe(Duration::from_secs(15)) {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("basic_length=3"),
        "StringBuilder.length() after 3 appends should be 3 — got:\n{combined}"
    );
    assert!(
        combined.contains("basic_toString=abc"),
        "StringBuilder.toString() after appending 'a','b','c' should be \
         \"abc\" — got:\n{combined}"
    );
    assert!(
        combined.contains("grown_length=40"),
        "StringBuilder.length() after 40 appends (past default capacity \
         16) should be 40 — got:\n{combined}"
    );
    assert!(
        combined.contains("padded_length=20"),
        "the length-keyed growth loop (while (sb.length() < 20) \
         sb.append('x')) should terminate with length 20 — got:\n{combined}"
    );
    assert!(
        combined.contains("DONE"),
        "probe did not reach its final marker — got:\n{combined}"
    );
}
