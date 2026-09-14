// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test for `ArrayList.listIterator(int).previous()`. JUnit Platform
//! uses this path to notify engine listeners in reverse order; a stale snapshot
//! array in CratonVM's native listIterator override surfaced as a null listener.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.util.*;

public class ArrayListReverseListIteratorProbe {
    public static void main(String[] args) {
        ArrayList<String> list = new ArrayList<>();
        list.add("A");
        list.add("B");
        ListIterator<String> it = list.listIterator(list.size());
        String first = it.hasPrevious() ? it.previous() : "NO_PREV";
        String second = it.hasPrevious() ? it.previous() : "NO_PREV";
        boolean done = !it.hasPrevious();
        if (!"B".equals(first) || !"A".equals(second) || !done) {
            throw new AssertionError(
                "bad reverse iterator: first=" + first + " second=" + second + " done=" + done);
        }
        System.out.println("ARRAYLIST_REVERSE_LIST_ITERATOR_OK");
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
                    panic!("ArrayListReverseListIteratorProbe timed out after {timeout:?}");
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
fn arraylist_list_iterator_previous_uses_live_snapshot() {
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
            eprintln!("javac not found; skipping ArrayListReverseListIteratorProbe");
            return;
        }
    };

    let temp = tempfile::tempdir().expect("temp probe dir");
    let source = temp.path().join("ArrayListReverseListIteratorProbe.java");
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
            eprintln!("[arraylist_reverse_list_iterator] javac could not be executed: {e}; skipping");
            return;
        }
        // javac RAN and rejected the source: the probe is broken, and skipping
        // here would make this test a permanent vacuous pass.
        Ok(o) => assert!(
            o.status.success(),
            "[arraylist_reverse_list_iterator] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ),
    }

    let child = Command::new(&bin)
        .arg("-c")
        .arg(temp.path())
        .arg("ArrayListReverseListIteratorProbe")
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
        combined.contains("ARRAYLIST_REVERSE_LIST_ITERATOR_OK"),
        "probe did not finish:\n{combined}"
    );
    assert!(
        !combined.contains("bad reverse iterator"),
        "reverse iterator regression reappeared:\n{combined}"
    );
}
