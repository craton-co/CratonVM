// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T2.11 — Real-app smoke tests.
//!
//! These tests verify that CratonVM can load and partially boot real-world
//! Java applications. They require external JAR files and/or a JDK 25
//! installation, so they are marked `#[ignore]` by default. Run them with:
//!
//!     cargo test -p cratonvm-vm --test t2_11_smoke_tests -- --ignored
//!
//! Environment variables:
//! - `JAVA_HOME`: path to JDK 25+ installation (for jimage/modules)
//! - `SPRING_BOOT_JAR`: path to a Spring Boot hello-world fat JAR
//! - `PETCLINIC_JAR`: path to the Spring Boot petclinic fat JAR
//! - `QUARKUS_JAR`: path to a Quarkus hello-world runner JAR
//!
//! These tests assert "no panic through the class loading + bootstrap
//! path" — they verify that the JVM infrastructure (class loading,
//! native method dispatch, module system) survives the boot sequence.

use std::sync::Arc;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::SharedVm;

/// Helper: create a SharedVm with a classpath and attempt to load a class.
/// Returns Ok(()) if class loading succeeds, Err with the failure message.
fn try_load_class(classpath: &[&str], class_name: &str) -> Result<(), String> {
    let config = VmConfig::new().with_classpath(classpath.iter().map(|s| s.to_string()).collect());
    let shared = Arc::new(SharedVm::new(config));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));

    // Attempt to resolve the class — this exercises the full class
    // loading pipeline (jimage reader, classpath scanner, Spring Boot
    // fat JAR extraction, native method registration).
    shared
        .load_class_concurrent(class_name)
        .map(|_| ())
        .map_err(|e| format!("Failed to load {class_name}: {e:?}"))
}

/// T2.11.1 — Boot a real Spring Boot 3 hello-world JAR.
///
/// Set `SPRING_BOOT_JAR` to the path of a Spring Boot fat JAR with
/// `Main-Class: org.springframework.boot.loader.JarLauncher` and
/// `Start-Class: com.example.demo.DemoApplication` (or similar).
#[test]
#[ignore = "requires SPRING_BOOT_JAR env var pointing to a real Spring Boot fat JAR"]
fn t2_11_1_spring_boot_hello_world() {
    let jar = std::env::var("SPRING_BOOT_JAR")
        .expect("Set SPRING_BOOT_JAR to a Spring Boot hello-world fat JAR path");
    assert!(std::path::Path::new(&jar).exists(), "JAR not found: {jar}");
    // Spring Boot's launcher class must be resolvable.
    let result = try_load_class(&[&jar], "org/springframework/boot/loader/JarLauncher");
    assert!(
        result.is_ok(),
        "Spring Boot hello-world boot failed: {}",
        result.unwrap_err()
    );
}

/// T2.11.2 — Boot Spring Boot petclinic far enough to resolve the launcher.
#[test]
#[ignore = "requires PETCLINIC_JAR env var pointing to the petclinic fat JAR"]
fn t2_11_2_spring_boot_petclinic() {
    let jar =
        std::env::var("PETCLINIC_JAR").expect("Set PETCLINIC_JAR to the petclinic fat JAR path");
    assert!(std::path::Path::new(&jar).exists(), "JAR not found: {jar}");
    let result = try_load_class(&[&jar], "org/springframework/boot/loader/JarLauncher");
    assert!(
        result.is_ok(),
        "Petclinic boot failed: {}",
        result.unwrap_err()
    );
}

/// T2.11.3 — Boot Quarkus hello world.
#[test]
#[ignore = "requires QUARKUS_JAR env var pointing to a Quarkus runner JAR"]
fn t2_11_3_quarkus_hello_world() {
    let jar = std::env::var("QUARKUS_JAR")
        .expect("Set QUARKUS_JAR to a Quarkus hello-world runner JAR path");
    assert!(std::path::Path::new(&jar).exists(), "JAR not found: {jar}");
    let result = try_load_class(&[&jar], "io/quarkus/runner/GeneratedMain");
    assert!(
        result.is_ok(),
        "Quarkus boot failed: {}",
        result.unwrap_err()
    );
}

/// T2.11.4 — Verify that javac's main class can be resolved from the JDK modules.
///
/// This doesn't run javac fully (which would require the full compiler
/// pipeline), but it verifies that `com.sun.tools.javac.Main` is loadable
/// from the JDK's module image, proving the jimage reader + module
/// resolution path works end-to-end.
#[test]
#[ignore = "requires JAVA_HOME env var pointing to JDK 25+ installation"]
fn t2_11_4_javac_class_resolution() {
    let java_home = std::env::var("JAVA_HOME").expect("Set JAVA_HOME to a JDK 25+ installation");
    let modules_path = format!("{java_home}/lib/modules");
    assert!(
        std::path::Path::new(&modules_path).exists(),
        "JDK modules not found at: {modules_path}"
    );
    let result = try_load_class(&[&modules_path], "com/sun/tools/javac/Main");
    assert!(
        result.is_ok(),
        "javac main class resolution failed: {}",
        result.unwrap_err()
    );
}

/// T2.11.5 — Verify that JShell's main class can be resolved from the JDK.
#[test]
#[ignore = "requires JAVA_HOME env var pointing to JDK 25+ installation"]
fn t2_11_5_jshell_class_resolution() {
    let java_home = std::env::var("JAVA_HOME").expect("Set JAVA_HOME to a JDK 25+ installation");
    let modules_path = format!("{java_home}/lib/modules");
    assert!(
        std::path::Path::new(&modules_path).exists(),
        "JDK modules not found at: {modules_path}"
    );
    let result = try_load_class(
        &[&modules_path],
        "jdk/internal/jshell/tool/JShellToolProvider",
    );
    assert!(
        result.is_ok(),
        "JShell main class resolution failed: {}",
        result.unwrap_err()
    );
}
