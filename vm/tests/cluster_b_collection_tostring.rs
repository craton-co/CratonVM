// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cluster-B / Session 107 — collection toString regression test.
//!
//! Pins the fix in `vm/src/runtime/interpreter.rs` and `vm/src/vm/vm_exec.rs`
//! that prevents the parent-class native walk from short-circuiting through
//! `java/lang/Object.toString` (which produces `ClassName@hash`) when an
//! intermediate ancestor (e.g. `java/util/AbstractCollection`) has its own
//! bytecode for the method.
//!
//! Before the fix, `arrayList.toString()` printed `java.util.ArrayList@<hash>`
//! in real-JDK mode because the slow-path walk found `Object.toString` (a
//! Rust native registered in `register_essential_natives`) before letting the
//! inherited `AbstractCollection.toString()` bytecode dispatch.
//!
//! After the fix, the walk stops at the first ancestor that defines its own
//! bytecode for the (name, descriptor), so `[1, 2, 3]` is produced via the
//! real JDK iteration. Pins all four collection types (ArrayList,
//! Arrays$ArrayList, HashSet, HashMap) the affected apps surfaced.
//!
//! Probe source: `apps/collection_tostring_probe/CollProbe.java`.
//! Affected apps fixed by the same change: `apps/ejbca_min_fixture/Main`,
//! `apps/guava/Min2`, `apps/guava/Min3`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("collection_tostring_probe")
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

fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let cls = dir.join("CollProbe.class");
    if cls.exists() {
        return true;
    }
    let src = dir.join("CollProbe.java");
    if !src.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "cluster_b_collection_tostring",
            "the Cluster B fixture `CollProbe.java`",
            &[src.clone()],
        );
        return false;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&src)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[cluster_b_collection_tostring] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[cluster_b_collection_tostring] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            cls.exists()
        }
    }
}

/// Prerequisite gate: the lookup below is unchanged — only a MISSING JDK is
/// reported differently. See `common::require_jdk`.
fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default.exists() {
        return Some(default);
    }
    None
}

#[test]
fn collection_tostring_matches_hotspot_format() {
    if !ensure_probe_compiled() {
        eprintln!("[cluster_b_collection_tostring] CollProbe.class unavailable; skipping");
        return;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[cluster_b_collection_tostring] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[cluster_b_collection_tostring] no JDK home \
                 (set CRATONVM_TEST_JDK or JAVA_HOME); skipping"
            );
            return;
        }
    };
    let classes = probe_dir();
    let mut child = match Command::new(&bin)
        .arg("--java-home")
        .arg(&jdk)
        .arg("-c")
        .arg(&classes)
        .arg("CollProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[cluster_b_collection_tostring] failed to spawn cratonvm: {e}");
            return;
        }
    };
    let timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[cluster_b_collection_tostring] CollProbe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[cluster_b_collection_tostring] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Regression assertion: must not print the Object.toString fallback
    // ("ClassName@hash") for any of the four collection types.
    assert!(
        !stdout.contains("java.util.ArrayList@"),
        "ArrayList.toString() regressed to Object.toString() format.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("java.util.Arrays$ArrayList@"),
        "Arrays$ArrayList.toString() regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("java.util.HashSet@"),
        "HashSet.toString() regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("java.util.HashMap@"),
        "HashMap.toString() regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Positive assertions: each collection prints its expected JDK-spec form.
    assert!(
        stdout.contains("ArrayList=[1, 2, 3]"),
        "ArrayList toString missing.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Arrays.asList=[x, y, z]"),
        "Arrays.asList toString missing.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // HashSet ordering is not stable in HotSpot either; just require the
    // bracketed form with both elements.
    assert!(
        stdout.lines().any(|l| {
            l.starts_with("HashSet=[") && l.ends_with(']') && l.contains('a') && l.contains('b')
        }),
        "HashSet toString missing or wrong shape.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("HashMap={k=7}"),
        "HashMap toString missing.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "CollProbe did not reach OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "CollProbe exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}
