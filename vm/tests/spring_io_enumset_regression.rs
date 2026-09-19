// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Spring regression probes for response writer/reader bridges and json-smart
//! option collection setup. The Java probe mirrors the small failures that
//! blocked Spring mock servlet and JsonPath tests: `OutputStreamWriter.write(char[],
//! int, int)`, `InputStreamReader.close()`, and `EnumSet.addAll/copyOf` from an
//! arbitrary collection.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

public class SpringIoEnumSetRegressionProbe {
    enum Opt { A, B }

    public static void main(String[] args) throws Exception {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        OutputStreamWriter osw = new OutputStreamWriter(baos, StandardCharsets.UTF_8);
        osw.write(new char[] {'O', 'K'}, 0, 2);
        osw.flush();
        if (!"OK".equals(baos.toString("UTF-8"))) {
            throw new AssertionError("bad writer result: " + baos.toString("UTF-8"));
        }

        InputStreamReader reader = new InputStreamReader(
                new ByteArrayInputStream("x".getBytes(StandardCharsets.UTF_8)),
                StandardCharsets.UTF_8);
        reader.close();
        System.out.println("STREAM_WRITER_READER_OK");

        EnumSet<Opt> set = EnumSet.noneOf(Opt.class);
        boolean changed = set.addAll(Arrays.asList(Opt.A, Opt.B));
        if (!changed || set.size() != 2 || !set.contains(Opt.A) || !set.contains(Opt.B)) {
            throw new AssertionError("bad addAll: changed=" + changed + " set=" + set);
        }
        EnumSet<Opt> copied = EnumSet.copyOf(Arrays.asList(Opt.A, Opt.B));
        if (copied.size() != 2 || !copied.contains(Opt.A) || !copied.contains(Opt.B)) {
            throw new AssertionError("bad copyOf: " + copied);
        }
        System.out.println("ENUMSET_COLLECTION_OK");
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

fn javac_bin() -> PathBuf {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    if let Ok(home) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join(exe);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(exe)
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
                    panic!("SpringIoEnumSetRegressionProbe timed out after {timeout:?}");
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
fn spring_response_io_and_enumset_collection_bridges() {
    let bin = match cratonvm_binary() {
        Some(bin) => bin,
        None => {
            eprintln!("cratonvm binary not found; build `cargo build -p cratonvm-cli`");
            return;
        }
    };

    let temp = tempfile::tempdir().expect("temp probe dir");
    let source = temp.path().join("SpringIoEnumSetRegressionProbe.java");
    std::fs::File::create(&source)
        .and_then(|mut f| f.write_all(PROBE_SRC.trim_start().as_bytes()))
        .expect("write probe source");

    let compile = Command::new(javac_bin())
        .arg("-d")
        .arg(temp.path())
        .arg(&source)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[spring_io_enumset_regression] javac could not be executed: {e}; skipping");
            return;
        }
        // javac RAN and rejected the source: the probe is broken, and skipping
        // here would make this test a permanent vacuous pass.
        Ok(o) => assert!(
            o.status.success(),
            "[spring_io_enumset_regression] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ),
    }

    let mut cmd = Command::new(&bin);
    if let Ok(home) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        cmd.arg("--java-home").arg(home);
    }
    let child = cmd
        .arg("-c")
        .arg(temp.path())
        .arg("SpringIoEnumSetRegressionProbe")
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
        combined.contains("STREAM_WRITER_READER_OK"),
        "writer/reader bridge probe did not finish:\n{combined}"
    );
    assert!(
        combined.contains("ENUMSET_COLLECTION_OK"),
        "EnumSet collection bridge probe did not finish:\n{combined}"
    );
}
