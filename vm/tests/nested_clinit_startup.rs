// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! real-cdi-bean-container Step 3 — Spring's nested startup-metrics `<clinit>`
//! chain runs on the real path by DEFAULT (no shim, no gate).
//!
//! Spring's `ApplicationStartup.DEFAULT` (`org.springframework.core.metrics`) is
//! a non-constant `static final` on an *interface*; its `<clinit>` runs
//! `new DefaultApplicationStartup()`, whose own `<clinit>` builds a
//! `static final DefaultStartupStep` singleton that in turn builds a
//! `DefaultTags`. The `spring_startup_bootstrap.rs` no-op shim that used to paper
//! over this nested concrete-class `<clinit>` chain — and the
//! `CRATONVM_SYNTHETIC_SPRING_STARTUP` opt-out gate that toggled it — have been
//! REMOVED. The real chain now completes on its own, unconditionally.
//!
//! This test pins that behavior with a framework-independent fixture
//! (`cratonvm/NestedClinitStartup`, real-JDK bytecode, JDK 25 / class major 69)
//! whose `Startup` / `DefaultStartup` / `DefaultStep` / `DefaultTags` classes
//! mirror Spring's `ApplicationStartup` / `DefaultApplicationStartup` /
//! `DefaultStartupStep` / `DefaultTags` byte-for-byte in `<clinit>` shape, so the
//! VM behavior is regression-tested in isolation from Spring. A single
//! `getstatic Startup.DEFAULT` (inside `probeNestedStartupClinit()`) drives the
//! whole nested `<clinit>` chain and the probe returns `true` — proving the real
//! chain completed WITHOUT any shim or swallow/`post_clinit_fixup` backfill.

#![cfg(not(feature = "synthetic-jdk"))]

use cratonvm_types::Value;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::Vm;

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

/// The real nested `<clinit>` chain (`Startup.<clinit>` → `new DefaultStartup`
/// → `DefaultStartup.<clinit>` → `new DefaultStep` → `new DefaultTags`) must
/// complete and yield a usable startup step on the DEFAULT path, with no shim
/// and no swallow + `post_clinit_fixup` backfill. `probeNestedStartupClinit()`
/// returns `true` iff the whole chain ran and the singleton step is usable
/// (non-null DEFAULT, `start()` non-null, `getName()=="default"`, `getTags()` a
/// non-null empty iterable).
#[test]
fn real_nested_startup_clinit_completes() {
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
             the real nested <clinit> chain failed (it must run without any shim)"
        ),
    };

    assert!(
        ok,
        "probeNestedStartupClinit() returned false — the nested \
         DefaultStartup.<clinit> (which builds the DefaultStep singleton + \
         DefaultTags) did not complete on the real path. There is no shim and no \
         swallow/fixup backfill any more, so a half-initialized singleton or null \
         DEFAULT means the real chain is broken."
    );
}
