// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cluster C — `java.lang.reflect.Constructor.newInstance` end-to-end.
//!
//! Pins the 11/11 PASS outcome of `apps/constructor_probe/ConstructorProbe`,
//! which exercises the eleven JLS §16 / §15.9.1 edge cases that the
//! `Constructor.newInstance` native must honor:
//!
//!   1. Public no-arg
//!   2. Public primitive args
//!   3. Public Object args
//!   4. Private constructor (IllegalAccessException without setAccessible)
//!   5. Constructor that throws — wrapped in InvocationTargetException
//!      with `getCause().getMessage()` preserved
//!   6. Abstract class -> InstantiationException
//!   7. Interface -> InstantiationException
//!   8. Inner class (non-static) — implicit outer reference is the
//!      first descriptor parameter
//!   9. Record canonical constructor
//!  10. Record compact-canonical with validation body that runs BEFORE
//!      the implicit field assignments
//!  11. Generic varargs constructor
//!
//! The two cases that previously regressed in real-JDK mode were #5 and
//! #10. Both traced to a layout mismatch between our Throwable native
//! handlers (which assumed `detailMessage` at slot 0 and `cause` at slot
//! 1, the synthetic-stub layout) and the real JDK 25 Throwable layout
//! (`backtrace` at slot 0, `detailMessage` at slot 1, `cause` at slot
//! 2, plus `target` on `InvocationTargetException` which overrides
//! `getCause()`). The fix lives in
//! `native-builtins/src/lang_misc.rs` — the {get,write}_throwable_*
//! helpers prefer field-name-based access with slot fallback so both
//! layouts work.
//!
//! Test #10 additionally verified that `java/lang/Record.<init>()V`
//! returns `Ok(None)` for a void native instead of pushing a stray
//! `Object(None)` onto the operand stack — that bug shifted the
//! caller record's stack depth and silently mis-dispatched the
//! validation `athrow`.
//!
//! Subprocess pattern with a hard 60s timeout (HotSpot finishes in
//! ~0.3 s, cratonvm release in ~5 s). On regression the test fails with
//! the per-case `fail-N-...` line surfaced from the probe.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("constructor_probe")
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

/// Compile `ConstructorProbe.java` via `javac` if any class file is missing.
/// Returns false only when javac cannot be LAUNCHED; a javac that runs and
/// rejects the fixture panics (see `probe_compile_guard.rs`).
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("ConstructorProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("ConstructorProbe.java");
    if !source.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "cluster_c_constructor",
            "the Cluster C fixture `ConstructorProbe.java` (the test pins its 11/11 PASS output)",
            &[source.clone()],
        );
        return false;
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
                        "[cluster_c_constructor] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[cluster_c_constructor] the checked-in probe fixture failed to compile — fix the \
                 .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// Run `ConstructorProbe` through the cratonvm binary with a hard timeout.
/// Returns `Some((stdout, stderr, exit_code))` on successful spawn, `None`
/// if pre-requisites are missing (so the caller can `return` and skip).
fn run_constructor_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    if !ensure_probe_compiled() {
        eprintln!(
            "[cluster_c_constructor] ConstructorProbe.class unavailable \
             (javac on PATH?); skipping"
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[cluster_c_constructor] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return None;
        }
    };
    let dir = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("-c")
        .arg(&dir)
        .arg("ConstructorProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[cluster_c_constructor] failed to spawn cratonvm: {e}");
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
                        "[cluster_c_constructor] ConstructorProbe timed out \
                         after {timeout:?} — likely an infinite loop in the \
                         reflection dispatch path"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[cluster_c_constructor] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[cluster_c_constructor] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    ))
}

/// Cache the subprocess output across all sub-tests so we pay the VM
/// bootstrap cost only once.
fn cached_run() -> Option<(String, String, Option<i32>)> {
    static CELL: OnceLock<Option<(String, String, Option<i32>)>> = OnceLock::new();
    CELL.get_or_init(|| run_constructor_probe(Duration::from_secs(60)))
        .clone()
}

#[test]
fn constructor_probe_full_pass() {
    let (stdout, stderr, code) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("summary 11/11"),
        "ConstructorProbe regressed. Expected 'summary 11/11' in:\n{combined}"
    );
    assert!(
        combined.contains("\nOK\n") || combined.ends_with("\nOK"),
        "ConstructorProbe did not print 'OK' marker after summary in:\n{combined}"
    );
    // `apps/constructor_probe/ConstructorProbe.java` deliberately does not
    // call System.exit. If the VM ever propagates a non-zero exit on a
    // clean main() return that surfaces here.
    assert!(
        matches!(code, Some(0) | None),
        "ConstructorProbe exited with non-zero code {code:?} despite \
         summary 11/11. Combined output:\n{combined}"
    );
}

/// Test #5 (throws-in-ctor): the wrapping must preserve
/// `cause.getMessage()`. Pre-fix this surfaced as
/// `fail-5-throws cause=RuntimeException msg=null` — the
/// `native_throwable_get_message` slot-0 read missed the real-JDK
/// `detailMessage` field (slot 1 in the `backtrace`/`detailMessage`/
/// `cause` layout).
#[test]
fn case_5_throws_preserves_cause_message() {
    let (stdout, stderr, _) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("pass-5-throws"),
        "Test 5 (constructor throws) regressed. Expected 'pass-5-throws' \
         in:\n{combined}"
    );
    assert!(
        !combined.contains("fail-5-throws"),
        "Test 5 (constructor throws) regressed with explicit fail line in:\n{combined}"
    );
}

/// Test #10 (record compact-canonical): the validation throw inside the
/// compact body must reach the caller, wrapped as
/// `InvocationTargetException` whose `getCause()` is the IAE. Pre-fix
/// this surfaced as `fail-10-record-compact good=5 rejected=false` —
/// `Throwable.getCause` native intercepted ITE's overridden bytecode
/// `getCause` and returned the inherited `cause` slot (null) instead
/// of the `target` field. The `Record.<init>()V` native was also
/// returning `Some(Object(None))` for a void method, pushing a stray
/// null on the operand stack.
#[test]
fn case_10_record_compact_validation_throws() {
    let (stdout, stderr, _) = match cached_run() {
        Some(o) => o,
        None => return,
    };
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("pass-10-record-compact"),
        "Test 10 (record compact-canonical) regressed. Expected \
         'pass-10-record-compact' in:\n{combined}"
    );
    assert!(
        !combined.contains("fail-10-record-compact"),
        "Test 10 (record compact-canonical) regressed with explicit fail \
         line in:\n{combined}"
    );
}
