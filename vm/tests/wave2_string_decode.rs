// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 2 (S108) — String decode regression test.
//!
//! Pins the fix in `vm/src/vm/vm_object.rs::read_java_string_inner`.
//!
//! ## The bug
//! When a non-String object's first instance field is an array whose
//! element type is neither `Char` nor `Byte` (e.g. Guava's `ImmutableList`
//! whose `field 0` is the backing `Object[]`), the old `_ =>` arm of the
//! `match elem_type` block called `read_char_array_bulk` and returned
//! `Some(garbage)` — the raw header/element bytes interpreted as UTF-16.
//!
//! Callers (`value_to_string` in `runtime/invokedynamic.rs`,
//! `invoke_to_string` in `native-builtins/src/lang_string.rs`) treat any
//! `Some(_)` from `read_java_string` as "this is already a String — use
//! it as-is". The garbage short-circuit therefore prevented the normal
//! `toString()` virtual dispatch from running, producing output like
//! `xs=㊘粹ƈ` instead of `xs=[a, b, c]` for `String.valueOf(immutableList)`,
//! `"xs=" + immutableList`, and `System.out.println(immutableList)`.
//!
//! After the fix the `_` arm returns `None`, which lets callers fall
//! through to `invoke_virtual("toString", ...)` and read back the proper
//! `[a, b, c]` String the JDK's `AbstractCollection.toString()` builds.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("string_decode_probe")
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

fn ensure_probe_compiled(name: &str) -> bool {
    let dir = probe_dir();
    let cls = dir.join(format!("{name}.class"));
    if cls.exists() {
        return true;
    }
    let src = dir.join(format!("{name}.java"));
    if !src.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave2_string_decode",
            &format!("the Wave 2 string-decode fixture `{name}.java`"),
            &[src.clone()],
        );
        return false;
    }
    let compile = Command::new("javac").arg("-d").arg(&dir).arg(&src).output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            assert!(
                o.status.success(),
                "[wave2_string_decode] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            cls.exists()
        }
    }
}

fn run_probe(name: &str) -> Option<(String, String, std::process::ExitStatus)> {
    if !ensure_probe_compiled(name) {
        eprintln!("[wave2_string_decode] probe {name}.class unavailable; skipping");
        return None;
    }
    let bin = cratonvm_binary()?;
    let jdk = jdk_home()?;
    let classes = probe_dir();
    let mut child = Command::new(&bin)
        .arg("--java-home")
        .arg(&jdk)
        .arg("-c")
        .arg(&classes)
        .arg(name)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[wave2_string_decode] {name} timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[wave2_string_decode] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Some((stdout, stderr, output.status))
}

#[test]
fn deep_list_tostring_concat_and_valueof() {
    let (stdout, stderr, status) = match run_probe("DeepListProbe") {
        Some(t) => t,
        None => return,
    };
    assert!(
        stdout.contains("t=[a, b, c]"),
        "direct toString() concat regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("xs=[a, b, c]"),
        "SCF concat with object regressed (the S107/Wave 2 garble).\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.lines().any(|l| l.trim_end() == "[a, b, c]"),
        "println(Object) regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("v=[a, b, c]"),
        "String.valueOf(Object) regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "DeepListProbe did not reach OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        status.success(),
        "DeepListProbe exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status
    );
}

#[test]
fn int_list_tostring_concat_and_valueof() {
    let (stdout, stderr, status) = match run_probe("IntListProbe") {
        Some(t) => t,
        None => return,
    };
    assert!(
        stdout.contains("t=[1, 2, 3]"),
        "direct toString() concat regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("xs=[1, 2, 3]"),
        "SCF concat with object regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.lines().any(|l| l.trim_end() == "[1, 2, 3]"),
        "println(Object) regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("v=[1, 2, 3]"),
        "String.valueOf(Object) regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "IntListProbe did not reach OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        status.success(),
        "IntListProbe exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status
    );
}

#[test]
fn str_decode_basic_round_trip() {
    let (stdout, stderr, status) = match run_probe("StrDecode") {
        Some(t) => t,
        None => return,
    };
    assert!(
        stdout.contains("latin=abcde"),
        "plain latin string regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("sb=xs=[a, b, c]"),
        "StringBuilder.toString concat regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("concat=xs=[a, b, c]"),
        "literal+literal concat regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "StrDecode did not reach OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        status.success(),
        "StrDecode exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status
    );
}
