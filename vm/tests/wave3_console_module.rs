// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 3 — `Console.instantiateConsole` / `Module.canUse(Class)` regression.
//!
//! Pins the fix for the NPE that surfaced inside
//! `java.io.Console.instantiateConsole()` once the S108 `bytebuddy_probe`
//! agent enabled the static-init path through `ServiceLoader.checkCaller`.
//!
//! The Java-level `Module.canUse(Class)` body reads `this.descriptor` —
//! a field that does not exist on our synthetic 2-field Module shape
//! (name=0, layer=1) — and dereferences it, producing a NullPointer
//! deep inside `ServiceLoader.checkCaller(Class, Class)`. We register a
//! native override that returns `true` (every module is treated as
//! permissively `uses`-declared), with the spec-correct edge cases:
//! null-receiver returns `false`, null service-class throws NPE.
//!
//! Acceptance:
//!
//! 1. The triple `(java/lang/Module, canUse, (Ljava/lang/Class;)Z)` is
//!    present in `shared.natives.native_methods` after VM construction.
//! 2. Subprocess: `apps/console_probe/ConsoleProbe.java` (`System.console()`
//!    on a non-TTY) prints `console=null` + `OK` and exits 0 under
//!    `CRATONVM_STRICT_SWALLOWS=1`. Any regression that lets the
//!    `Module.canUse` NPE escape `ServiceConfigurationError` would fail
//!    here — strict-swallows surfaces panics that the loop's catch
//!    block would otherwise hide.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::SharedVm;

fn shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

#[test]
fn module_canuse_class_is_registered() {
    let shared = shared();
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Module", "canUse", "(Ljava/lang/Class;)Z")
            .is_some(),
        "Wave3 regression: Module.canUse(Class) MUST be registered as a \
         native — the real bytecode dereferences `this.descriptor`, a \
         field that does not exist on our synthetic Module shape, which \
         crashes ServiceLoader.checkCaller during Console.instantiateConsole."
    );
}

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

fn java_home() -> Option<PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&jh);
        if p.exists() {
            return Some(p);
        }
    }
    let candidate = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

fn console_probe_dir() -> PathBuf {
    workspace_root().join("apps").join("console_probe")
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

fn ensure_console_probe_compiled() -> bool {
    let dir = console_probe_dir();
    let class_file = dir.join("ConsoleProbe.class");
    let source = dir.join("ConsoleProbe.java");
    if !source.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave3_console_module",
            "the Wave 3 console fixture `ConsoleProbe.java`",
            &[source.clone()],
        );
        return false;
    }
    if up_to_date(&class_file, &source) {
        return true;
    }
    let compile = Command::new("javac")
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
            assert!(
                o.status.success(),
                "[wave3_console_module] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

fn run_console_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    if !ensure_console_probe_compiled() {
        eprintln!(
            "wave3_console_module: ConsoleProbe.class missing and javac unavailable; skipping"
        );
        return None;
    }
    let bin = cratonvm_binary()?;
    let jh = java_home()?;
    let dir = console_probe_dir();

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home")
        .arg(&jh)
        .arg("-c")
        .arg(&dir)
        .arg("ConsoleProbe")
        .env("CRATONVM_STRICT_SWALLOWS", "1");

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
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
    };
    let out = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Some((stdout, stderr, status.code()))
}

#[test]
fn console_probe_no_canuse_npe_under_strict_swallows() {
    let Some((stdout, stderr, rc)) = run_console_probe(Duration::from_secs(120)) else {
        eprintln!(
            "wave3_console_module: prerequisites missing; skipping \
             (set CRATONVM_BIN + JAVA_HOME or build target/release/cratonvm)"
        );
        return;
    };
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        combined.contains("console=null"),
        "ConsoleProbe must print `console=null` (matches HotSpot reference \
         on a non-TTY); got:\n{combined}"
    );
    assert!(
        combined.contains("OK"),
        "ConsoleProbe must print `OK` after System.console() — any \
         Module.canUse NPE escaping ServiceConfigurationError stops the \
         probe before this line; got:\n{combined}"
    );
    assert_eq!(
        rc,
        Some(0),
        "ConsoleProbe must exit 0 under CRATONVM_STRICT_SWALLOWS=1"
    );
}
