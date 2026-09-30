// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RKC16N.12 — `java/lang/management/ManagementFactory.<clinit>` regression.
//!
//! Pin the fix that lets `ManagementFactory` initialize without raising
//! `UnsatisfiedLinkError` during the JBoss Modules / Keycloak boot path.
//!
//! Before this fix, `<clinit>` of `sun.management.VMManagementImpl`
//! (instantiated as a static field of `ManagementFactoryHelper`, which is
//! itself reachable from the `ManagementFactory.<clinit>` chain via
//! `MethodHandles.lookup().ensureInitialized(ManagementFactory.class)`)
//! had three thread counters registered with the wrong descriptor:
//!
//!   - `getLiveThreadCount()I`  was registered as `()J`
//!   - `getPeakThreadCount()I`  was registered as `()J`
//!   - `getDaemonThreadCount()I` was registered as `()J`
//!
//! plus two natives missing entirely:
//!
//!   - `getUptime0()J`
//!   - `getAvailableProcessors()I`
//!
//! Either gap surfaces as an UnsatisfiedLinkError when JMM init walks the
//! VMManagementImpl method table during platform-MBean enumeration.
//!
//! This test does NOT spawn a real subprocess (Keycloak boot is owned by
//! `bench/keycloak/` and the sibling agents that co-evolve with this
//! native surface). Instead, it loads the class in-process with the
//! standard `Vm` harness and asserts:
//!
//!   1. `swallow_counter` does not increment for the ManagementFactory
//!      class init path,
//!   2. the class is in `Initialized` state after load + a method call.
//!
//! On regression (descriptor mismatch reintroduced, or new natives added
//! to VMManagementImpl in a JDK update), this test fails with the
//! `class=java/lang/management/ManagementFactory exc=java/lang/UnsatisfiedLinkError`
//! marker line and a non-zero swallow counter.

#![cfg(not(feature = "synthetic-jdk"))]

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::Vm;

/// Real JDK 25 install — only run when JAVA_HOME points at a real JDK.
fn java_home() -> Option<std::path::PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = std::path::PathBuf::from(&jh);
        if p.join("lib").join("modules").exists() {
            return Some(p);
        }
    }
    // Adoptium 25 default install — matches the worktree environment.
    let default =
        std::path::PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default.join("lib").join("modules").exists() {
        return Some(default);
    }
    None
}

#[test]
fn management_factory_clinit_no_swallow() {
    let Some(jh) = java_home() else {
        eprintln!(
            "Skipping: real JDK 25 not found. Set JAVA_HOME or install \
             Adoptium at the default path."
        );
        return;
    };

    let config = VmConfig::new().with_java_home(jh.to_string_lossy().into_owned());
    let mut vm = Vm::new(config);
    let baseline = vm
        .shared
        .debug
        .swallow_counter
        .load(std::sync::atomic::Ordering::Relaxed);

    // Triggers the full ManagementFactory.<clinit> chain plus
    // ManagementFactoryHelper -> new VMManagementImpl() -> getVersion0()
    // + initOptionalSupportFields(). Any UnsatisfiedLinkError during
    // VMManagementImpl method-table linking surfaces here as a swallow
    // recorded against ManagementFactory's class init.
    let load_result = vm.load_class("java/lang/management/ManagementFactory");
    assert!(
        load_result.is_ok(),
        "load_class(ManagementFactory) returned {:?}",
        load_result
    );

    let after = vm
        .shared
        .debug
        .swallow_counter
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        after, baseline,
        "swallow_counter incremented during ManagementFactory class init \
         (baseline={baseline}, after={after}). The most likely cause is a \
         missing or wrong-descriptor native on sun/management/VMManagementImpl. \
         Re-run with CRATONVM_STRICT_SWALLOWS=1 for a backtrace pinpointing \
         the exact native."
    );
}
