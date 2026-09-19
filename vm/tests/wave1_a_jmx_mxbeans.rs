// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 1 / Task A — `ManagementFactory.getXxxMXBeans()` regression test.
//!
//! Pre-fix `ManagementFactory.getMemoryPoolMXBeans()`,
//! `getMemoryManagerMXBeans()`, and `getGarbageCollectorMXBeans()` all
//! returned empty lists in real-JDK mode because the underlying
//! `sun/management/MemoryImpl.getMemoryPools0()` /
//! `getMemoryManagers0()` natives returned zero-length arrays. Spring
//! Actuator, JConsole, and Prometheus exporters all iterate these lists;
//! an empty list short-circuits health probes and exporters drop their
//! gauges silently.
//!
//! Post-fix the natives return one or more populated `MemoryPoolImpl` /
//! `GarbageCollectorImpl` instances tagged with the right name + type.
//! The probe at `apps/jmx_probe/JmxProbe.java` exercises the exact
//! `ManagementFactory` getters Spring Actuator uses.

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
        .join("jmx_probe")
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

fn compile_jmx_probe(probe: &Path) -> bool {
    let source = probe.join("JmxProbe.java");
    if !source.exists() {
        // `apps/jmx_probe/JmxProbe.java` IS tracked (force-added past the
        // `.gitignore` `apps/` rule), so its absence means a broken checkout, not
        // an absent toolchain. Loud, and a failure under CRATONVM_REQUIRE_E2E —
        // see `common::require_fixture`.
        let _ = common::require_fixture(
            "wave1_a_jmx",
            "the Wave 1 Task A fixture `JmxProbe.java` (tracked at apps/jmx_probe/JmxProbe.java \
             despite the `apps/` gitignore rule)",
            &[source.clone()],
        );
        return false;
    }
    let javac = java_home()
        .map(|home| {
            PathBuf::from(home)
                .join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
        })
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "javac.exe" } else { "javac" }));
    match Command::new(javac).arg(&source).current_dir(probe).output() {
        Ok(out) => {
            // javac RAN and rejected the fixture: answering `false` here reads
            // to the caller as "javac unavailable, skip", which makes this test
            // a permanent vacuous pass.
            assert!(
                out.status.success(),
                "[wave1_a_jmx] the checked-in probe fixture failed to compile — fix \
                 the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            true
        }
        // javac cannot be launched at all — the one legitimate skip.
        Err(error) => {
            eprintln!("[wave1_a_jmx] failed to launch javac: {error}; skipping");
            false
        }
    }
}
fn run_jmx_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !compile_jmx_probe(&probe) {
        return None;
    }
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("JmxProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave1_a_jmx] failed to spawn cratonvm: {e}");
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
                    panic!("[wave1_a_jmx] JmxProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave1_a_jmx] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave1_a_jmx] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

/// Pin: `ManagementFactory.getMemoryPoolMXBeans()` returns at least one
/// pool, `getMemoryManagerMXBeans()` returns at least one manager,
/// `getGarbageCollectorMXBeans()` returns at least one GC bean. The
/// probe terminates with the literal `OK` line on success.
#[test]
fn jmx_probe_returns_nonempty_mxbean_lists() {
    let (stdout, stderr, rc) = match run_jmx_probe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[wave1_a_jmx] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "wave1_a_jmx: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("listener=OK"),
        "wave1_a_jmx: MemoryMXBean listener registration must complete. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("OK"),
        "wave1_a_jmx: JmxProbe must reach the final `OK` line. Got stdout={:?}",
        stdout
    );

    // pools=N where N >= 1
    let pools_n = parse_count(&stdout, "pools=");
    assert!(
        pools_n.is_some_and(|n| n >= 1),
        "wave1_a_jmx: getMemoryPoolMXBeans() must return >= 1 pool. \
         Parsed pools={:?}, stdout={:?}",
        pools_n,
        stdout
    );

    let mgrs_n = parse_count(&stdout, "mgrs=");
    assert!(
        mgrs_n.is_some_and(|n| n >= 1),
        "wave1_a_jmx: getMemoryManagerMXBeans() must return >= 1 manager. \
         Parsed mgrs={:?}, stdout={:?}",
        mgrs_n,
        stdout
    );

    let gcs_n = parse_count(&stdout, "gcs=");
    assert!(
        gcs_n.is_some_and(|n| n >= 1),
        "wave1_a_jmx: getGarbageCollectorMXBeans() must return >= 1 GC \
         bean. Parsed gcs={:?}, stdout={:?}",
        gcs_n,
        stdout
    );
}

fn parse_count(stdout: &str, prefix: &str) -> Option<u32> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().parse().ok();
        }
    }
    None
}
