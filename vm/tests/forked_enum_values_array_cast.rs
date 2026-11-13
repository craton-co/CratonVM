// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Spring `@CompileWithForkedClassLoader` regression: an enum's compiler-generated
//! `values()` (`$VALUES.clone()` then `checkcast [LE;`) threw
//! `ClassCastException: [LE; cannot be cast to [LE;` in the application loader once a
//! child loader had defined its own copy of `E`, under `--compatible` only
//! (`AutowiredAnnotationBeanRegistrationAotContributionTests`, 5 of 14 tests).
//!
//! The application class's `[LE;` constant resolved by name alone, so the child loader's
//! array class (the only one of that name the store then held) answered for it.
//! `vm/tests/resources/forkenum/ForkEnumProbe.java` runs the fork's `values()` first, then
//! the application's, and prints what HotSpot prints. Both modes must match.
//!
//! Compiles the probe with the host JDK's `javac`; skips (or fails under
//! `CRATONVM_REQUIRE_E2E=1`) without the binary or the JDK.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(binary_lookup())
}

fn binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(bin);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) { "cratonvm.exe" } else { "cratonvm" };
    ["release", "debug"]
        .iter()
        .map(|profile| manifest_dir().parent().unwrap().join("target").join(profile).join(exe))
        .find(|p| p.exists())
}

fn java_home() -> Option<PathBuf> {
    common::require_jdk(
        ["CRATONVM_JAVA_HOME", "JAVA_HOME"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .map(PathBuf::from)
            .find(|h| h.join("bin").join(if cfg!(windows) { "javac.exe" } else { "javac" }).exists()),
    )
}

fn javac(home: &Path, out: &Path, sources: &[&str]) -> bool {
    let res = manifest_dir().join("tests").join("resources").join("forkenum");
    Command::new(home.join("bin").join("javac"))
        .arg("-d")
        .arg(out)
        .args(sources.iter().map(|s| res.join(s)))
        .status()
        .is_ok_and(|s| s.success())
}

/// stdout of the probe under `mode`, or `None` when a prerequisite is missing.
fn run_probe(mode: &str) -> Option<String> {
    let bin = cratonvm_binary()?;
    let home = java_home()?;
    let work = std::env::temp_dir().join(format!("forkenum-{}-{}", std::process::id(), mode.trim_start_matches('-')));
    let (app, fork) = (work.join("app"), work.join("fork"));
    std::fs::create_dir_all(&app).ok()?;
    std::fs::create_dir_all(&fork).ok()?;
    // The fork's copy of E (and F, which calls E.values()) must be bytes the app path never loads.
    assert!(javac(&home, &app, &["E.java", "ForkEnumProbe.java"]), "failed to compile the app probe (javac)");
    assert!(javac(&home, &fork, &["E.java", "F.java"]), "failed to compile the fork probe (javac)");
    let child = Command::new(bin)
        .arg(mode)
        .arg("--java-home")
        .arg(&home)
        .arg("-cp")
        .arg(&app)
        .arg("ForkEnumProbe")
        .arg(&fork)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let out = common::wait_draining(child, Duration::from_secs(120)).output;
    let _ = std::fs::remove_dir_all(&work);
    Some(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

fn assert_hotspot_answer(mode: &str) {
    let Some(text) = run_probe(mode) else {
        eprintln!("[forkenum] skipping {mode}: cratonvm binary or JDK with javac not available");
        return;
    };
    assert!(
        text.contains("fork values component is fork E = true") && text.contains("app values = 2"),
        "{mode}: an enum's values() in the application loader must not see the fork's [LE;\n{text}"
    );
}

#[test]
fn compatible_application_values_ignores_forked_array_class() {
    assert_hotspot_answer("--compatible");
}

#[test]
fn jdk_only_application_values_ignores_forked_array_class() {
    assert_hotspot_answer("--real-jdk");
}
