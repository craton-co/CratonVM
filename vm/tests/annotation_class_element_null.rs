// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `Annotation.annotationType()` and `Class`-valued annotation members must
//! never read back as `null`.
//!
//! Three unrelated frameworks dereference these without a null check, and each
//! one crashed on a CratonVM null (see
//! `annotation-class-element-resolves-null-cluster-RETIRED-20260805.md`):
//!
//! | caller | expression that NPEs on a null |
//! |---|---|
//! | Spring `AnnotationsScanner.isIgnorable` | `annotation.annotationType().getName()` |
//! | JUnit `AnnotationUtils.findRepeatableAnnotations` | `candidateAnnotationType.equals(containerType)` |
//! | Byte Buddy `ForFieldBinding.bind` | `declaringType(annotation).represents(void.class)` |
//!
//! The probe (`tests/resources/annclassprobe/AnnClassProbe.java`) covers the
//! four shapes those callers walk — direct annotations, a `@Repeatable`
//! container's `value()` entries, the meta-annotations ON an annotation type,
//! and an OMITTED `Class`-valued member whose declared default is `void.class`
//! — each of them twice, the second time through a child loader that defines
//! its own copies (so the annotation type name has more than one definition and
//! the loader-blind global lookup is ambiguous).
//!
//! `AcpGoneTarget` covers the case that actually escaped: its `@Repeatable`
//! container is loadable but the CONTAINED annotation type is not (its
//! `.class` lives in `compileonly/`, off the runtime classpath — the
//! `org.apiguardian.api.API` shape). The container's `value()` array used to
//! come back holding proxies with a null `annotationType()`, because the
//! nested-annotation element path never consulted the loadability filter that
//! the top-level array builders use.
//!
//! Runs the real-JDK `cratonvm` binary. Skips gracefully when the binary,
//! compiled probe classes, or a host JDK are unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn probe_dir() -> PathBuf {
    manifest_dir()
        .join("tests")
        .join("resources")
        .join("annclassprobe")
}

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
    for var in ["CRATONVM_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(h) = std::env::var(var) {
            if Path::new(&h).exists() {
                return Some(h);
            }
        }
    }
    // Any JDK 25 the box happens to carry; the probe uses no JDK-version-
    // specific API, it only needs a real `java.base`.
    for candidate in [
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
        "/data/jdk25-real-20260717/jdk-25.0.3+9",
    ] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Run `main_class` from the probe directory under CratonVM.
///
/// The classpath is the probe directory ONLY — `compileonly/` is deliberately
/// left off it, which is what makes `AcpGone` unresolvable at runtime.
fn run_probe(main_class: &str, timeout: Duration) -> Option<(String, String)> {
    let bin = cratonvm_binary()?;
    let dir = probe_dir();
    if !dir.join(format!("{main_class}.class")).exists() {
        eprintln!("[ann-class-null] {main_class}.class missing — skipping");
        return None;
    }
    let dir_str = dir.display().to_string();
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg(format!("-Dprobe.dir={dir_str}"));
    cmd.arg("-cp").arg(&dir_str).arg(main_class);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ann-class-null] failed to spawn cratonvm: {e}");
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
    ))
}

/// Every annotation reachable by reflection reports a non-null
/// `annotationType()`, and an omitted `Class`-valued member reports its
/// declared `void.class` default — app loader and child loader alike.
#[test]
fn annotation_type_and_class_members_are_never_null() {
    let Some((stdout, stderr)) = run_probe("AnnClassProbe", Duration::from_secs(120)) else {
        eprintln!(
            "[ann-class-null] skipping — cratonvm binary, compiled probe classes, \
             or host JDK not available"
        );
        return;
    };
    assert!(
        stdout.contains("SUMMARY: S1=PASS S2=PASS S3=PASS S4=PASS S5=PASS"),
        "annotation type/Class-member probe did not pass.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// A `@Repeatable` container whose CONTAINED annotation type is absent from the
/// runtime classpath must not yield entries whose `annotationType()` is null.
///
/// HotSpot raises `NoClassDefFoundError` from `getDeclaredAnnotations()` itself
/// here (`AnnotationType.<init>` resolves the container's own `AcpGone[]`
/// member descriptor eagerly). CratonVM is allowed to be laxer — surfacing the
/// container and deferring the failure to member ACCESS, the same
/// `TypeNotPresentException` representation an unresolvable `Class`-valued
/// member already uses — but it is NOT allowed to hand back a live annotation
/// whose type is null, which is what every walker in this cluster crashed on.
#[test]
fn unresolvable_contained_annotation_type_never_surfaces_as_a_null_type() {
    let Some((stdout, stderr)) = run_probe("AcpGoneProbe", Duration::from_secs(120)) else {
        eprintln!(
            "[ann-class-null] skipping — cratonvm binary, compiled probe classes, \
             or host JDK not available"
        );
        return;
    };
    assert!(
        stdout.contains("GONE-SUMMARY: nullTypes=0 nonAnnotations=0"),
        "an unresolvable annotation type surfaced with a null annotationType(), \
         or as an element that is not a java.lang.annotation.Annotation.\
         \nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Name the directly-applied case (`AcpGoneSolo`, added 2026-08-06). The
    // summary above is a sum: it read `nullTypes=0` for months while that shape
    // was broken, because nothing exercised it.
    //
    // There the annotation admission filter resolved the type name through
    // `load_class`, which FABRICATES a stand-in for a name on no classpath — so
    // the filter could essentially never say "unresolvable", and
    // `getDeclaredAnnotations()` returned a `Proxy` over a non-interface: an
    // element that is not an `Annotation` at all. Byte Buddy casts every
    // element to `Annotation`, so Mockito's inline mock maker could not modify
    // the class, and for a FINAL class (nothing to subclass) `mock()` failed
    // outright — `org.infinispan.query.remote.client.impl.QueryRequest` and its
    // `@org.jboss.marshalling.Externalize`.
    assert!(
        stdout.contains("AcpGoneSolo.getDeclaredAnnotations()"),
        "the probe never reached the directly-applied case, so the summary \
         proves nothing about it.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
