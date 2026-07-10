// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test for
//! `docs/known-issues/wildfly-jboss-modules-service-provider-leak.md`.
//!
//! Drives the exact call WildFly's boot code uses to discover a module's
//! `Extension` implementations —
//! `org.jboss.as.controller.extension.ExtensionAddHandler.initializeExtension`
//! resolves each declared extension via
//! `Module.loadServiceFromCallerModuleLoader(moduleName, Extension.class)`
//! (verified against the constant pool of `ExtensionAddHandler.class` in
//! `wildfly-controller-24.0.1.Final.jar`) — against a real WildFly
//! distribution's `modules/` tree.
//!
//! `org.jboss.as.jmx` and `org.wildfly.extension.core-management` are two
//! unrelated modules (neither depends on the other) that each ship their
//! own `META-INF/services/org.jboss.as.controller.Extension` descriptor.
//! A module-scoped `ServiceLoader` must return exactly the calling module's
//! own provider; the WFLYCTL0226 "subsystem already registered" boot
//! failure is the observable symptom of a leak where one module's lookup
//! also picks up the other's provider.
//!
//! Requires a local WildFly 32.0.1.Final distribution unpacked at
//! `WILDFLY_DIST_MODULES` (default: `/data/data/wildfly-dist/wildfly-32.0.1.Final/modules`)
//! plus `javac` on `PATH`; skips (rather than fails) when either is absent
//! so this doesn't break environments without the real distribution staged.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests").join("wildfly_boot_fixtures")
}

fn wildfly_dist_modules() -> PathBuf {
    std::env::var("WILDFLY_DIST_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/data/wildfly-dist/wildfly-32.0.1.Final/modules"))
}

/// The `org.jboss.as.controller` module's jar — needed on the probe's own
/// classpath so `Class.forName("org.jboss.as.controller.Extension")`
/// resolves without going through module-scoped class loading (the probe
/// itself isn't loaded by any ModuleClassLoader).
fn controller_jar(modules_root: &Path) -> Option<PathBuf> {
    let dir = modules_root.join("system/layers/base/org/jboss/as/controller/main");
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find_map(|e| {
            let p = e.path();
            let is_jar = p.extension().and_then(|s| s.to_str()) == Some("jar");
            is_jar.then_some(p)
        })
}

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

/// Two-step compile mirroring `vm/tests/wp8_10_jboss_modules_smoke.rs`:
/// the `org/jboss/modules/*` stubs compile to a side directory that is
/// NOT on the runtime classpath, so at runtime those symbols resolve to
/// the synthetic stub shapes cratonvm's native registry backs — not the
/// compile-time stub bytecode.
fn ensure_probe_compiled() -> bool {
    let dir = fixture_dir();
    let probe_class = dir.join("cratonvm/wildfly/JBossModuleServiceLeakProbe.class");
    if probe_class.exists() {
        return true;
    }
    let stubs_classes = dir.join("stubs_classes");
    let _ = std::fs::create_dir_all(&stubs_classes);
    let stub_status = Command::new("javac")
        .arg("-d")
        .arg(&stubs_classes)
        .arg(dir.join("stubs/org/jboss/modules/Module.java"))
        .arg(dir.join("stubs/org/jboss/modules/ModuleLoader.java"))
        .arg(dir.join("stubs/org/jboss/modules/ModuleClassLoader.java"))
        .arg(dir.join("stubs/org/jboss/modules/LocalModuleLoader.java"))
        .arg(dir.join("stubs/org/jboss/modules/DefaultBootModuleLoaderHolder.java"))
        .status();
    if !matches!(stub_status, Ok(s) if s.success()) {
        return false;
    }
    let probe_status = Command::new("javac")
        .arg("-cp")
        .arg(&stubs_classes)
        .arg("-d")
        .arg(&dir)
        .arg(dir.join("JBossModuleServiceLeakProbe.java"))
        .status();
    matches!(probe_status, Ok(s) if s.success()) && probe_class.exists()
}

/// Runs the probe for one module, returning the comma-joined provider
/// class names it discovered (or the `ERROR:...` string the probe emits
/// on a caught Throwable).
fn run_probe(
    bin: &Path,
    modules_root: &Path,
    cp: &str,
    module_name: &str,
    service: &str,
) -> String {
    let output = Command::new(bin)
        .env("CRATONVM_JBOSS_MP_ROOT", modules_root)
        .arg("-cp")
        .arg(cp)
        .arg("cratonvm.wildfly.JBossModuleServiceLeakProbe")
        .arg(module_name)
        .arg(service)
        .output()
        .expect("failed to spawn cratonvm");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("RESULT:") {
            return rest.to_string();
        }
    }
    panic!(
        "probe for module={module_name} produced no RESULT: line.\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );
}

#[test]
fn jboss_module_service_provider_lookup_is_module_scoped() {
    let modules_root = wildfly_dist_modules();
    if !modules_root.is_dir() {
        eprintln!(
            "[wildfly_jboss_module_service_leak] {} not present; skipping \
             (set WILDFLY_DIST_MODULES to a WildFly modules/ tree to run this test)",
            modules_root.display()
        );
        return;
    }
    let Some(ctrl_jar) = controller_jar(&modules_root) else {
        eprintln!(
            "[wildfly_jboss_module_service_leak] org.jboss.as.controller jar not found; skipping"
        );
        return;
    };
    if !ensure_probe_compiled() {
        eprintln!(
            "[wildfly_jboss_module_service_leak] probe compile failed (javac on PATH?); skipping"
        );
        return;
    }
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[wildfly_jboss_module_service_leak] cratonvm binary not found; \
             build with `cargo build --release -p cratonvm-cli`"
        );
        return;
    };

    let cp = format!("{}:{}", fixture_dir().display(), ctrl_jar.display());
    const SERVICE: &str = "org.jboss.as.controller.Extension";

    let jmx = run_probe(&bin, &modules_root, &cp, "org.jboss.as.jmx", SERVICE);
    let cm = run_probe(
        &bin,
        &modules_root,
        &cp,
        "org.wildfly.extension.core-management",
        SERVICE,
    );

    assert_eq!(
        jmx, "org.jboss.as.jmx.JMXExtension",
        "org.jboss.as.jmx's Extension ServiceLoader lookup should return \
         exactly its own provider; got {jmx:?}. A leaked \
         org.wildfly.extension.core.management.CoreManagementExtension entry \
         here reproduces WFLYCTL0226."
    );
    assert_eq!(
        cm, "org.wildfly.extension.core.management.CoreManagementExtension",
        "org.wildfly.extension.core-management's Extension ServiceLoader \
         lookup should return exactly its own provider; got {cm:?}. A \
         leaked org.jboss.as.jmx.JMXExtension entry here reproduces \
         WFLYCTL0226."
    );
}
