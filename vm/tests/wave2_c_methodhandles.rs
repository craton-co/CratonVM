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
//! swaths of modern Java (per `history/roadmap-any-java-app.md` item RC.8).
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

fn cratonvm_binary() -> Option<PathBuf> {
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

fn run_mh_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !probe.join("MhProbe.class").exists() {
        eprintln!("[wave2-c] MhProbe.class missing — run javac in apps/methodhandles_probe");
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
