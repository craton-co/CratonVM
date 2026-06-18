// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! real-cdi-bean-container increment 2 (Step 2) — the nested concrete-class
//! `<clinit>` under the `CRATONVM_REAL_SPRING_STARTUP` gate.
//!
//! Spring's `ApplicationStartup.DEFAULT` (`org.springframework.core.metrics`) is
//! a non-constant `static final` on an *interface*; its `<clinit>` runs
//! `new DefaultApplicationStartup()`, whose own `<clinit>` builds a
//! `static final DefaultStartupStep` singleton that in turn builds a
//! `DefaultTags`. That NESTED concrete-class `<clinit>` chain is what the
//! `spring_startup_bootstrap.rs` shim papers over: by default the chain is
//! swallowed + backfilled by `post_clinit_fixup` and every
//! `getApplicationStartup()/start()/tag()/end()` is force-routed to a no-op
//! native.
//!
//! Increment 2 makes the real chain run behind `CRATONVM_REAL_SPRING_STARTUP`
//! (default OFF). When the flag is ON, the no-op natives are not registered, the
//! `getApplicationStartup`/`start`/`tag`/`end` `check_override` force arms are
//! suppressed, and `ApplicationStartup` is dropped from the lenient
//! `<clinit>`-swallow allowlist + its `post_clinit_fixup` arm is skipped — so the
//! real `<clinit>` chain must complete on its own.
//!
//! This test pins that behavior with a framework-independent fixture
//! (`cratonvm/NestedClinitStartup`, real-JDK bytecode, JDK 25 / class major 69)
//! whose `Startup` / `DefaultStartup` / `DefaultStep` / `DefaultTags` classes
//! mirror Spring's `ApplicationStartup` / `DefaultApplicationStartup` /
//! `DefaultStartupStep` / `DefaultTags` byte-for-byte in `<clinit>` shape, so the
//! VM fix is regression-tested in isolation from Spring.
//!
//! - `flag-ON` (in-process): with `CRATONVM_REAL_SPRING_STARTUP` set, a single
//!   `getstatic Startup.DEFAULT` (inside `probeNestedStartupClinit()`) drives the
//!   whole nested `<clinit>` chain and the probe returns `true` — proving the
//!   real chain completed WITHOUT the swallow/fixup shim.
//! - `flag-OFF` (subprocess, env-isolated): with the var unset, the existing
//!   fallback (shim + swallow + fixup) still completes the probe.
//!
//! The flag-ON case is in-process (like `iface_static_final_init.rs`) and runs
//! first / alone so the process-lifetime `OnceLock` gate caches the ON value
//! deterministically; the flag-OFF case is a subprocess so its environment is
//! isolated from that cache.

#![cfg(not(feature = "synthetic-jdk"))]

use cratonvm_types::Value;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::Vm;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!(
        "{dir}/cratonvm/NestedClinitStartup$DefaultStartup.class"
    ))
    .exists()
}

/// Resolve a real JDK 25 install (the VM still needs `java.base` for `Object`,
/// `String`, `Collections`, etc. when running app fixtures). Mirrors
/// `iface_static_final_init.rs`.
fn java_home() -> Option<std::path::PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = std::path::PathBuf::from(&jh);
        if p.join("lib").join("modules").exists() {
            return Some(p);
        }
    }
    let default =
        std::path::PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default.join("lib").join("modules").exists() {
        return Some(default);
    }
    None
}

/// FLAG ON — `CRATONVM_REAL_SPRING_STARTUP` set: the real nested `<clinit>`
/// chain (`Startup.<clinit>` → `new DefaultStartup` → `DefaultStartup.<clinit>`
/// → `new DefaultStep` → `new DefaultTags`) must complete and yield a usable
/// startup step, WITHOUT the no-op shim or the swallow + `post_clinit_fixup`
/// backfill. `probeNestedStartupClinit()` returns `true` iff the whole chain ran
/// and the singleton step is usable (non-null DEFAULT, `start()` non-null,
/// `getName()=="default"`, `getTags()` a non-null empty iterable).
#[test]
fn real_nested_startup_clinit_completes_when_flag_on() {
    if !fixture_available() {
        eprintln!("Skipping: NestedClinitStartup fixture not compiled");
        return;
    }
    let Some(jh) = java_home() else {
        eprintln!(
            "Skipping: real JDK 25 not found. Set JAVA_HOME or install \
             Adoptium at the default path."
        );
        return;
    };

    // Drive the gate ON for this process. This test runs in-process and is the
    // only one in this file that depends on the cached `real_spring_startup()`
    // gate, so the OnceLock caches the ON value deterministically.
    std::env::set_var("CRATONVM_REAL_SPRING_STARTUP", "1");

    let config = VmConfig::new()
        .with_java_home(jh.to_string_lossy().into_owned())
        .with_classpath(vec![test_resources_dir()]);
    let mut vm = Vm::new(config);

    let result = vm.invoke(
        "cratonvm/NestedClinitStartup",
        "probeNestedStartupClinit",
        "()Z",
        &[],
    );

    let ok = match result {
        Ok(Some(Value::Int(v))) => v != 0,
        other => panic!(
            "probeNestedStartupClinit() did not return a boolean: {other:?} — \
             the real nested <clinit> chain failed under \
             CRATONVM_REAL_SPRING_STARTUP (it should run without the shim)"
        ),
    };

    assert!(
        ok,
        "probeNestedStartupClinit() returned false under \
         CRATONVM_REAL_SPRING_STARTUP — the nested DefaultStartup.<clinit> \
         (which builds the DefaultStep singleton + DefaultTags) did not complete \
         on the real path. With the gate ON there is no swallow/fixup backfill, \
         so a half-initialized singleton or null DEFAULT means the real chain \
         is still broken."
    );
}

// ── flag-OFF fallback (subprocess, env-isolated) ────────────────────────────

fn cratonvm_binary() -> Option<PathBuf> {
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

/// OPT-OUT — `CRATONVM_SYNTHETIC_SPRING_STARTUP=1`: the legacy fallback path
/// (no-op startup-metrics shim + lenient `<clinit>` swallow + `post_clinit_fixup`
/// backfill) must still complete the probe. real-cdi-bean-container increment 3
/// flipped the real path to the DEFAULT, so the shim is now reached only via
/// this opt-out. Driven as a subprocess so its environment is isolated from the
/// in-process gate cache above.
///
/// Skips gracefully when the `cratonvm` binary has not been built (the
/// orchestrator builds centrally) — matching `cluster_a_aqs_chm.rs`.
#[test]
fn shimmed_startup_still_works_when_flag_off() {
    if !fixture_available() {
        eprintln!("Skipping: NestedClinitStartup fixture not compiled");
        return;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[nested_clinit_startup] cratonvm binary not found; skipping \
                 flag-OFF fallback check (build with \
                 `cargo build --release -p cratonvm-cli`)"
            );
            return;
        }
    };
    let Some(jh) = java_home() else {
        eprintln!(
            "[nested_clinit_startup] real JDK 25 not found; skipping flag-OFF \
             fallback check"
        );
        return;
    };

    let dir = test_resources_dir();
    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home")
        .arg(jh.to_string_lossy().into_owned())
        .arg("-c")
        .arg(&dir)
        .arg("cratonvm.NestedClinitStartup")
        // real-cdi-bean-container increment 3: the real startup-metrics path is
        // now the DEFAULT, so opt OUT to the legacy shim with
        // CRATONVM_SYNTHETIC_SPRING_STARTUP, and clear the (now-redundant) real
        // opt-in so a stray export can't override the opt-out.
        .env_remove("CRATONVM_REAL_SPRING_STARTUP")
        .env("CRATONVM_SYNTHETIC_SPRING_STARTUP", "1")
        // The legacy shimmed Spring path relies on the lenient <clinit> swallow.
        .env("CRATONVM_LENIENT_CLINIT", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[nested_clinit_startup] failed to spawn cratonvm: {e}");
            return;
        }
    };
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[nested_clinit_startup] flag-OFF subprocess timed out");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[nested_clinit_startup] try_wait failed: {e}");
                return;
            }
        }
    }
    let output = child.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n--- STDERR ---\n{stderr}");

    assert!(
        combined.contains("NESTED_STARTUP_OK"),
        "flag-OFF (shim + swallow + fixup) path did not complete the probe — \
         expected 'NESTED_STARTUP_OK'. Output:\n{combined}"
    );
}
