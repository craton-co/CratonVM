// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test for synthetic `Collections.synchronized*` wrappers: their
//! inherited `mutex` field must be initialized before pure-JDK methods such as
//! `forEach` execute `synchronized (mutex)`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.util.*;
import java.util.concurrent.atomic.AtomicInteger;

public class SyncWrapperMutexProbe {
    public static void main(String[] args) {
        ArrayList<String> list = new ArrayList<>();
        list.add("a");
        list.add("b");

        Collection<String> c = Collections.synchronizedCollection(list);
        AtomicInteger collectionCount = new AtomicInteger();
        c.forEach(s -> collectionCount.addAndGet(s.length()));
        if (collectionCount.get() != 2) {
            throw new AssertionError("collection count=" + collectionCount.get());
        }
        System.out.println("collection-ok");

        Set<String> set = Collections.synchronizedSet(new LinkedHashSet<>(list));
        AtomicInteger setCount = new AtomicInteger();
        set.forEach(s -> setCount.incrementAndGet());
        if (setCount.get() != 2) {
            throw new AssertionError("set count=" + setCount.get());
        }
        System.out.println("set-ok");

        Map<String, String> map = Collections.synchronizedMap(new LinkedHashMap<>());
        map.put("k", "v");
        AtomicInteger mapCount = new AtomicInteger();
        map.forEach((k, v) -> mapCount.incrementAndGet());
        if (mapCount.get() != 1) {
            throw new AssertionError("map count=" + mapCount.get());
        }
        System.out.println("map-ok");
        System.out.println("SYNC_WRAPPER_MUTEX_OK");
    }
}
"#;

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
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = manifest.parent()?.join("target").join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn javac_bin() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    if let Ok(home) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    Some(PathBuf::from(exe))
}

fn run_with_timeout(mut child: std::process::Child, timeout: Duration) -> Option<(String, String)> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("SyncWrapperMutexProbe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

#[test]
fn synchronized_wrappers_initialize_mutex_for_for_each() {
    let bin = match cratonvm_binary() {
        Some(bin) => bin,
        None => {
            eprintln!("cratonvm binary not found; build `cargo build -p cratonvm-cli`");
            return;
        }
    };
    let javac = match javac_bin() {
        Some(javac) => javac,
        None => {
            eprintln!("javac not found; skipping SyncWrapperMutexProbe");
            return;
        }
    };

    let temp = tempfile::tempdir().expect("temp probe dir");
    let source = temp.path().join("SyncWrapperMutexProbe.java");
    std::fs::File::create(&source)
        .and_then(|mut f| f.write_all(PROBE_SRC.trim_start().as_bytes()))
        .expect("write probe source");

    let compile = Command::new(&javac)
        .arg("-d")
        .arg(temp.path())
        .arg(&source)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[synchronized_wrapper_mutex] javac could not be executed: {e}; skipping");
            return;
        }
        // javac RAN and rejected the source: the probe is broken, and skipping
        // here would make this test a permanent vacuous pass.
        Ok(o) => assert!(
            o.status.success(),
            "[synchronized_wrapper_mutex] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ),
    }

    let child = Command::new(&bin)
        .arg("-c")
        .arg(temp.path())
        .arg("SyncWrapperMutexProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cratonvm");
    let (stdout, stderr) = match run_with_timeout(child, Duration::from_secs(30)) {
        Some(out) => out,
        None => return,
    };
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("collection-ok"),
        "missing collection marker:\n{combined}"
    );
    assert!(
        combined.contains("set-ok"),
        "missing set marker:\n{combined}"
    );
    assert!(
        combined.contains("map-ok"),
        "missing map marker:\n{combined}"
    );
    assert!(
        combined.contains("SYNC_WRAPPER_MUTEX_OK"),
        "probe did not finish:\n{combined}"
    );
    assert!(
        !combined.contains("this.mutex\" is null") && !combined.contains("this.mutex is null"),
        "null mutex regression reappeared:\n{combined}"
    );
}
