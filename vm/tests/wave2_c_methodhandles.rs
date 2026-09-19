// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 2, Task C — `apps/methodhandles_probe/MhProbe.java` regression test.
//!
//! Pin the JDK 11+ `java.lang.invoke.MethodHandles.Lookup` API end-to-end:
//! `findStatic` / `findVirtual` across the {primitive, object, varargs} ×
//! {user class, JDK class} matrix. Spawns the cratonvm CLI in real-JDK mode
//! and asserts the seven probe lines plus the final `OK`.
//!
//! This is the JDK-11+ replacement for reflection that every modern logging
//! framework (Log4j 2 LMC), `StringConcatFactory.makeConcatWithConstants`,
//! `LambdaMetafactory.metafactory`, JSON libraries, and the JVM's own
//! bootstrap-method invocation path rely on. Coverage gaps here break wide
//! swaths of modern Java (per `roadmap-any-java-app.md` item RC.8).
//!
//! Required output (HotSpot reference, all 8 lines):
//!   findStatic.prim=7
//!   findStatic.obj=HELLO
//!   findStatic.varargs=10
//!   findVirtual.prim=30
//!   findVirtual.obj=hi!
//!   findStatic.jdk=42
//!   findVirtual.jdk=5
//!   OK
//!
//! Per the wave-2-C method, the test passes if *at least 5 of 7* lookup
//! lines match the HotSpot reference AND the trailing `OK` is printed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn worktree_root() -> PathBuf {
    manifest_dir().parent().unwrap().to_path_buf()
}

fn probe_dir() -> PathBuf {
    worktree_root().join("apps").join("methodhandles_probe")
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
    let target = worktree_root().join("target");
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

/// Is the compiled class OLDER than the fixture source?
///
/// A `.class` that predates its `.java` is the documented stale-artefact trap:
/// the compiled probe keeps answering a question the current source no longer
/// asks, so an edit to the fixture appears in no log. An unreadable timestamp
/// (no source at all, no metadata) is not evidence of staleness — and cannot be
/// repaired by recompiling either — so it reads as `false` and any existing
/// artefact is kept.
fn class_older_than_source(cls: &Path, src: &Path) -> bool {
    let Ok(src_mtime) = src.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    match cls.metadata().and_then(|m| m.modified()) {
        Ok(cls_mtime) => cls_mtime < src_mtime,
        // Cannot tell — recompile rather than trust it.
        Err(_) => true,
    }
}

/// Compile `MhProbe.java` unless an up-to-date `MhProbe.class` is already there.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let cls = dir.join("MhProbe.class");
    let src = dir.join("MhProbe.java");
    if cls.exists() && !class_older_than_source(&cls, &src) {
        return true;
    }
    if !src.exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`. `apps/` is gitignored (.gitignore line 12),
        // which is why this fixture was never tracked.
        let _ = common::require_fixture(
            "wave2-c",
            "the Wave 2 Task C fixture `MhProbe` (MhProbe.class, compiled from MhProbe.java)",
            &[cls.clone(), src.clone()],
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
        // javac RAN and rejected the fixture: skipping here would make this test
        // a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported
            // `--release` means this javac never opened the file, which is a
            // missing-toolchain condition rather than a broken probe.
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[wave2-c] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wave2-c] the checked-in probe fixture failed to compile — fix the .java source. \
                 javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            cls.exists()
        }
    }
}

fn run_mh_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !ensure_probe_compiled() {
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("MhProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave2-c] failed to spawn cratonvm: {e}");
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
                    panic!("[wave2-c] MhProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave2-c] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave2-c] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

#[test]
fn methodhandles_probe_matches_hotspot_matrix() {
    let (stdout, stderr, rc) = match run_mh_probe(Duration::from_secs(120)) {
        Some(o) => o,
        None => {
            eprintln!("[wave2-c] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "wave2-c: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );

    // The seven HotSpot-reference lines, in order.  The probe constructs each
    // method-handle lookup independently, so a partial failure can leave a
    // subset of lines correct — the wave-2-C method requires at least 5 of
    // the 7 to match.
    let expected: [&str; 7] = [
        "findStatic.prim=7",
        "findStatic.obj=HELLO",
        "findStatic.varargs=10",
        "findVirtual.prim=30",
        "findVirtual.obj=hi!",
        "findStatic.jdk=42",
        "findVirtual.jdk=5",
    ];
    let matched: usize = expected.iter().filter(|s| stdout.contains(*s)).count();
    assert!(
        matched >= 5,
        "wave2-c: only {} of 7 MethodHandle lookups matched HotSpot. \
         expected (any 5+ of) {:?}, got stdout={:?}",
        matched,
        expected,
        stdout
    );
    assert!(
        stdout.contains("OK"),
        "wave2-c: MhProbe must reach the final `OK` line. Got stdout={:?}",
        stdout
    );
}
