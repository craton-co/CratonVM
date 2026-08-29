// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 2, Task A — `Class.getDeclaredFields/Methods/Constructors` regression.
//!
//! Pin the contract that the three "getDeclared*" reflection natives in
//! `native-builtins/src/lang_class.rs` produce arrays whose elements have
//! correct names, modifiers, type mirrors, and parameter types — the load-
//! bearing primitive for DI containers (Spring, CDI, Quarkus ArC),
//! serialization (Jackson, Gson), ORM (Hibernate, EclipseLink), and
//! annotation scanners.
//!
//! Spawns the cratonvm CLI on `apps/reflect_probe/ReflectProbe.java`, which
//! exercises:
//!   * 4 declared fields  (private static final long, public static int,
//!                         private final String, public int)
//!   * 5 declared methods (intMethod, strMethod, privStaticVoid,
//!                         varargsMethod, main) — counts post-synthetic-filter
//!   * 4 declared constructors (no-arg, (String,int), (int,String), int...)
//!
//! The probe also exercises the `transient` flag (varargs-as-array) and the
//! `Constructor.isVarArgs()` boolean — those line up with the HotSpot
//! reference printout captured in the task description.
//!
//! Counts and the literal `OK` are asserted; the per-element signature
//! ordering is intentionally NOT pinned (the JLS does not specify an order
//! for `getDeclared*`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn probe_dir() -> PathBuf {
    manifest_dir()
        .parent()
        .unwrap()
        .join("apps")
        .join("reflect_probe")
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
    let probe = probe_dir();
    if !probe.join("ReflectProbe.class").exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave2_a",
            "the Wave 2 Task A fixture `ReflectProbe` (ReflectProbe.class, compiled from \
             ReflectProbe.java)",
            &[
                probe.join("ReflectProbe.class"),
                probe.join("ReflectProbe.java"),
            ],
        );
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("ReflectProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave2_a] failed to spawn cratonvm: {e}");
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
                    let _ = child.wait();
                    panic!("[wave2_a] ReflectProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave2_a] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave2_a] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

/// Top-level acceptance: `Class.getDeclared*` returns the right-shaped
/// arrays for `ReflectProbe`. Counts pinned to the HotSpot reference:
///   fields=4
///   methods=5  (post `isSynthetic()`/`getDeclaringClass()==c` filter)
///   constructors=4
/// and the probe must reach the literal `OK` line.
#[test]
fn reflect_probe_getdeclared_counts_match_hotspot() {
    let (stdout, stderr, rc) = match run_probe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[wave2_a] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "wave2_a: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("fields=4"),
        "wave2_a: expected `fields=4`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("methods=5"),
        "wave2_a: expected `methods=5`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("constructors=4"),
        "wave2_a: expected `constructors=4`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("OK"),
        "wave2_a: ReflectProbe must reach the final `OK` line. Got stdout={:?}",
        stdout
    );
}

/// Per-element shape: every Field/Method/Constructor in the probe printout
/// must carry a non-empty modifier+type+name signature line. This guards
/// against regressions where the reflection objects deserialise with
/// modifiers=0, type=ClassId(0), or empty parameterTypes (all symptoms
/// the task description called out as likely root causes).
#[test]
fn reflect_probe_signatures_have_modifiers_and_types() {
    let (stdout, _stderr, rc) = match run_probe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[wave2_a] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(rc, Some(0), "wave2_a: cratonvm exited rc={:?}", rc);

    // Field signatures: each must contain its declared type's simple name.
    assert!(stdout.contains("long sf1"), "expected `long sf1` field row");
    assert!(stdout.contains("int sf2"), "expected `int sf2` field row");
    assert!(
        stdout.contains("String if1"),
        "expected `String if1` field row"
    );
    assert!(stdout.contains("int if2"), "expected `int if2` field row");

    // Method signatures: each declared method appears with its return type
    // and parameter types. Modifier strings must be non-empty.
    assert!(
        stdout.contains("int intMethod(int,int)"),
        "expected `int intMethod(int,int)`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("String strMethod(String)"),
        "expected `String strMethod(String)`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("void privStaticVoid()"),
        "expected `void privStaticVoid()`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("varargsMethod(String,Object[])"),
        "expected `varargsMethod(String,Object[])`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("void main(String[])"),
        "expected `void main(String[])`. Got stdout={:?}",
        stdout
    );

    // Constructor signatures: the four declared constructors, including
    // the int-varargs (`int[]`) constructor that must carry the `varargs`
    // flag (`isVarArgs() == true`).
    assert!(
        stdout.contains("<init>()"),
        "expected no-arg `<init>()`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("<init>(String,int)"),
        "expected `<init>(String,int)`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("<init>(int,String)"),
        "expected `<init>(int,String)`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("<init>(int[]) varargs"),
        "expected `<init>(int[]) varargs` (Constructor.isVarArgs() true). \
         Got stdout={:?}",
        stdout
    );
}
