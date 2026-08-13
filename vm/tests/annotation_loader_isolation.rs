// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Annotation Bug A — classloader-isolation `TypeNotPresentException`.
//!
//! Mirrors Spring's `AnnotationIntrospectionFailureTests`: a
//! `FilteringClassLoader` (OverridingClassLoader-style — redefines the test
//! classes under itself and throws `ClassNotFoundException` for any `*Filtered*`
//! type) loads `@AnnProbeExampleAnnotation(AnnProbeFilteredType.class)`-annotated
//! `AnnProbeWithAnnotation`. Reading the `Class`-valued `value()` must throw
//! `TypeNotPresentException` (cause `ClassNotFoundException`), because the member
//! is resolved through the declaring class's (filtering) loader — NOT the global
//! store. `getAnnotations()` itself must NOT throw (deferred, like HotSpot's
//! `TypeNotPresentExceptionProxy`).
//!
//! The probe (`tests/resources/annprobe/AnnProbe.java`) prints
//! `SUMMARY: S1=PASS S2=PASS S3=PASS S4=PASS S5=PASS S6=PASS` when:
//!   S1 the annotated class is loaded by the filtering loader,
//!   S2 `getAnnotations()` does not throw,
//!   S3 `value()` throws `TypeNotPresentException` with a `ClassNotFoundException` cause,
//!   S4 a *resolvable* Class member via the same loader still returns its class
//!      (no false `TypeNotPresentException`),
//!   S5 a *child-eligible* Class member resolves through the declaring class's
//!      loader, so `value().getClassLoader()` is that loader
//!      (mirrors `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`),
//!   S6 the SAME member read from an APPLICATION-loaded holder still answers
//!      with the application copy, even though S5 has left the child loader as
//!      the only one that has ever defined that name. A loader-blind
//!      "whoever has this name" fallback answers with the child's copy there —
//!      which is how an application-world log4j `@PluginAttribute` was handed a
//!      forked `PluginAttributeVisitor` and dropped every `<Logger>` element of
//!      its configuration.
//!
//! Runs the real-JDK `cratonvm` binary (this is a real-JDK class-loading +
//! reflection path). Skips gracefully when the binary, compiled probe classes,
//! or a host JDK are unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn probe_dir() -> PathBuf {
    manifest_dir()
        .join("tests")
        .join("resources")
        .join("annprobe")
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
    let target = manifest_dir().parent().unwrap().join("target");
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

fn java_home() -> Option<String> {
    if let Ok(h) = std::env::var("CRATONVM_JAVA_HOME") {
        return Some(h);
    }
    if let Ok(h) = std::env::var("JAVA_HOME") {
        return Some(h);
    }
    let candidate = "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot";
    if Path::new(candidate).exists() {
        return Some(candidate.to_string());
    }
    None
}

fn run_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let dir = probe_dir();
    if !dir.join("AnnProbe.class").exists() {
        return None;
    }
    let dir_str = dir.display().to_string();
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg(format!("-Dprobe.dir={dir_str}"));
    cmd.arg("-cp").arg(&dir_str).arg("AnnProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ann-loader] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    let mut child = child;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().ok()?;
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

/// Class-valued annotation members are resolved through the declaring class's
/// loader, so a FilteringClassLoader that rejects the referenced type yields a
/// deferred `TypeNotPresentException` (thrown at `value()`, not `getAnnotations()`).
#[test]
fn annotation_class_member_honors_declaring_loader_type_not_present() {
    let Some((stdout, stderr, _code)) = run_probe(Duration::from_secs(60)) else {
        eprintln!(
            "[ann-loader] skipping — cratonvm binary, compiled probe classes, \
             or host JDK not available"
        );
        return;
    };
    assert!(
        stdout.contains("SUMMARY: S1=PASS S2=PASS S3=PASS S4=PASS S5=PASS S6=PASS"),
        "annotation classloader-isolation probe did not pass.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
