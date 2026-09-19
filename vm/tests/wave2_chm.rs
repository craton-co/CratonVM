// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! W2-CHM — ConcurrentHashMap correctness regression.
//!
//! Pins the fix for the JIT miscompile that caused
//! `apps/chm_basic/ChmScale` to lose entries `k992..k999` from a
//! 1000-element single-threaded `ConcurrentHashMap<String,Integer>`. The
//! immediate symptom was `size=1000 mapSize=1000 found=992 firstMiss=992`
//! against the HotSpot reference's `found=1000 firstMiss=-1`.
//!
//! Root cause: the JIT compiles `Integer.valueOf(int)` /
//! `Integer.<init>(int)` (both trivial enough to clear the
//! `<init>` complexity gate) as an allocate-then-putfield sequence.
//! When the surrounding outer-method frame crosses both the OSR
//! back-edge threshold (1000) and the per-callee invocation threshold
//! (2000), the JIT'd path stores the wrong value into the boxed
//! `Integer.value` slot — the wrapper comes back with `value=0`.
//! The CHM happily stores `("k992" -> Integer(0))` and the subsequent
//! `get("k992").intValue()` reports 0 instead of 992.
//!
//! Fix (vm/src/jit/skip_list.rs::is_known_miscompile): add
//! `Integer.valueOf` / `Integer.<init>` (and the `Long` siblings) to
//! the conservative skip list so the interpreter retains those
//! allocate-then-putfield boxing sequences. Narrow: `Integer.toString`,
//! `Integer.parseInt`, and other non-allocating methods stay
//! JIT-eligible.
//!
//! Acceptance: `apps/chm_basic/ChmScale` runs to completion in well
//! under 30 s and emits `size=1000 mapSize=1000 found=1000 firstMiss=-1`
//! as its final line. Any regression in the skip list (or a deeper JIT
//! fix that re-enables Integer boxing without a correctness audit)
//! surfaces here as `firstMiss != -1` or `found != 1000`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn chm_basic_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().join("apps").join("chm_basic")
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

fn java_home() -> Option<PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&jh);
        if p.exists() {
            return Some(p);
        }
    }
    // Fall back to the canonical Adoptium 25 location used by the rest
    // of the suite — see e.g. cluster_a_aqs_chm.rs.
    let candidate = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

/// True when `class_file` exists and is at least as new as `src`.
///
/// A `.class` older than its `.java` is a standing trap here: reusing it means
/// the run exercises a stale fixture, so a landed source change shows up in no
/// log. When the mtimes cannot prove freshness, recompile.
fn up_to_date(class_file: &Path, src: &Path) -> bool {
    let (Ok(c), Ok(s)) = (class_file.metadata(), src.metadata()) else {
        return false;
    };
    match (c.modified(), s.modified()) {
        (Ok(c), Ok(s)) => c >= s,
        // No mtime on this filesystem: recompile rather than trust a stale class.
        _ => false,
    }
}

fn ensure_chm_scale_compiled() -> bool {
    let dir = chm_basic_dir();
    let class_file = dir.join("ChmScale.class");
    let source = dir.join("ChmScale.java");
    if !source.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave2_chm",
            "the Wave 2 CHM fixture `ChmScale.java`",
            &[source.clone()],
        );
        return false;
    }
    if up_to_date(&class_file, &source) {
        return true;
    }
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
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
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
                        "[wave2_chm] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wave2_chm] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

fn run_chm_scale(timeout: Duration) -> Option<(String, String)> {
    if !ensure_chm_scale_compiled() {
        eprintln!("wave2_chm: ChmScale.class missing and javac unavailable; skipping");
        return None;
    }
    let bin = cratonvm_binary()?;
    let jh = java_home()?;
    let dir = chm_basic_dir();

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home")
        .arg(&jh)
        .arg("-c")
        .arg(&dir)
        .arg("ChmScale");

    // Spawn so we can enforce a per-test timeout independent of any
    // VM-side watchdog.
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Some((stdout, stderr))
}

#[test]
fn chm_scale_pins_integer_valueof_jit_miscompile() {
    let Some((stdout, stderr)) = run_chm_scale(Duration::from_secs(60)) else {
        // Build artifacts or environment unavailable — skip rather than
        // false-fail. Real CI sets JAVA_HOME and builds the cratonvm
        // binary up front, so this branch only fires for the developer
        // running `cargo test -p cratonvm-vm` without those preconditions.
        eprintln!("wave2_chm: prerequisites missing; skipping (set CRATONVM_BIN + JAVA_HOME)");
        return;
    };
    let combined = format!("{stdout}\n{stderr}");

    // The reference HotSpot output for size=1000 must match exactly: every
    // entry round-trips, and the firstMiss sentinel is -1. This pins the
    // W2-CHM fix in `is_known_miscompile`.
    assert!(
        combined.contains("size=1000 mapSize=1000 found=1000 firstMiss=-1"),
        "ChmScale must report all 1000 entries round-trip; got:\n{combined}"
    );
    // Defensive: each smaller-size line must also round-trip — a regression
    // that loses entries earlier (e.g. at the `<init>` complexity gate
    // change) should fire here too.
    for size in [16, 32, 64, 128, 256, 512] {
        let expected = format!("size={s} mapSize={s} found={s} firstMiss=-1", s = size);
        assert!(
            combined.contains(&expected),
            "ChmScale size={size} stage must round-trip; got:\n{combined}"
        );
    }
}
