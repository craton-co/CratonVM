// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end custom-loader metadata reclamation gate.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
        .join("class_loader_unload")
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let path = PathBuf::from(bin);
        if path.exists() {
            return Some(path);
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    let executable = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    ["release", "debug"]
        .iter()
        .map(|profile| workspace.join("target").join(profile).join(executable))
        .find(|path| path.exists())
}

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| {
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
        })
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from("javac"))
}

fn compile_fixture(mode: &str) -> Option<(PathBuf, PathBuf)> {
    let output = std::env::temp_dir().join(format!(
        "cratonvm-class-unload-{}-{mode}",
        std::process::id(),
    ));
    let _ = std::fs::remove_dir_all(&output);
    std::fs::create_dir_all(&output).ok()?;
    let fixture = fixture_dir();
    let status = Command::new(javac())
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&output)
        .arg(fixture.join("LoaderUnloadPayload.java"))
        .arg(fixture.join("LoaderUnloadProbe.java"))
        .output();
    let status = match status {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!(
                "[class_loader_unload_regression] javac could not be executed: {e}; skipping"
            );
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !status.status.success() {
        let stderr_probe = String::from_utf8_lossy(&status.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[class_loader_unload_regression] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the fixture: skipping here would make this test a
    // permanent vacuous pass.
    assert!(
        status.status.success(),
        "[class_loader_unload_regression] the checked-in probe fixture failed to compile \
         — fix the .java source. javac stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let payload = output.join("unloadprobe").join("LoaderUnloadPayload.class");
    Some((output, payload))
}

fn run(mode: &str) {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[class_loader_unload_regression] cratonvm binary unavailable; skipping");
        return;
    };
    let Some((classes, payload)) = compile_fixture(mode) else {
        eprintln!("[class_loader_unload_regression] javac unavailable; skipping");
        return;
    };
    let mut command = Command::new(binary);
    if mode == "nojit" {
        command.arg("--nojit");
    }
    let mut child = command
        .arg("-c")
        .arg(&classes)
        .arg("LoaderUnloadProbe")
        .arg(&payload)
        .arg("6")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn class-loader unloading probe");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("poll class-loader unloading probe") {
            Some(_) => break,
            None if start.elapsed() < Duration::from_secs(180) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => {
                let _ = child.kill();
                panic!("class-loader unloading probe timed out in {mode} mode");
            }
        }
    }
    let output = child.wait_with_output().expect("collect probe output");
    let combined = format!(
        "{}\n--- STDERR ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{mode} probe failed:\n{combined}");
    assert!(
        combined.contains("liveLoaders=0")
            && combined.contains("liveClasses=0")
            && combined.contains("ok=true"),
        "{mode} probe did not reclaim loader metadata:\n{combined}"
    );
    let _ = std::fs::remove_dir_all(classes);
}

#[test]
fn custom_loader_metadata_is_reclaimed_with_jit() {
    run("jit");
}

#[test]
fn custom_loader_metadata_is_reclaimed_without_jit() {
    run("nojit");
}
