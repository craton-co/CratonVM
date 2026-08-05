// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for Java 11+ private methods encoded as `invokevirtual`.
//!
//! A superclass private instance helper and a subclass private static helper can
//! share the same name and descriptor because private methods are not inherited.
//! The VM must dispatch the superclass private call to the resolved constant-pool
//! target, not to the receiver class. Elasticsearch/Lucene hit this shape in
//! `BaseKnnVectorsFormatTestCase.add(...)` versus an Elasticsearch subclass
//! static helper with the same erased descriptor.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.util.Collections;

public class PrivateInvokevirtualShadowProbe {
    static final class Term {}

    static final class Field {}

    static final class Writer {
        int updates;

        long updateDocument(Term term, Iterable<Field> fields) {
            if (term == null) {
                throw new AssertionError("term was null");
            }
            if (!fields.iterator().hasNext()) {
                throw new AssertionError("fields were empty");
            }
            updates++;
            return 42L;
        }
    }

    static class Base {
        private final Writer writer = new Writer();
        private long observed;

        String run() {
            add(writer, "field", 7, Collections.singletonList(new Field()));
            if (observed != 42L) {
                throw new AssertionError("bad update result: " + observed);
            }
            if (writer.updates != 1) {
                throw new AssertionError("bad update count: " + writer.updates);
            }
            return "PRIVATE_INVOKEVIRTUAL_OK";
        }

        private void add(Writer writer, String field, int id, Iterable<Field> fields) {
            if (!"field".equals(field) || id != 7) {
                throw new AssertionError("bad args");
            }
            observed = writer.updateDocument(new Term(), fields);
        }
    }

    static class Child extends Base {
        private static void add(Writer writer, String field, int id, Iterable<Field> fields) {
            throw new AssertionError("WRONG_STATIC_SHADOW");
        }
    }

    public static void main(String[] args) {
        System.out.println(new Child().run());
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
                    panic!("PrivateInvokevirtualShadowProbe timed out after {timeout:?}");
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
fn private_invokevirtual_does_not_dispatch_to_subclass_static_shadow() {
    let bin = match cratonvm_binary() {
        Some(bin) => bin,
        None => {
            eprintln!("cratonvm binary not found; build `cargo build -p cratonvm-cli`");
            return;
        }
    };

    let temp = tempfile::tempdir().expect("temp probe dir");
    let source = temp.path().join("PrivateInvokevirtualShadowProbe.java");
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
            eprintln!("[private_invokevirtual_shadow] javac could not be executed: {e}; skipping");
            return;
        }
        // javac RAN and rejected the source: the probe is broken, and skipping
        // here would make this test a permanent vacuous pass.
        Ok(o) => assert!(
            o.status.success(),
            "[private_invokevirtual_shadow] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ),
    }

    let mut cmd = Command::new(&bin);
    if let Ok(home) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        cmd.arg("--java-home").arg(home);
    }
    let child = cmd
        .arg("--nojit")
        .arg("-c")
        .arg(temp.path())
        .arg("PrivateInvokevirtualShadowProbe")
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
        combined.contains("PRIVATE_INVOKEVIRTUAL_OK"),
        "private invokevirtual probe failed:\n{combined}"
    );
    assert!(
        !combined.contains("WRONG_STATIC_SHADOW"),
        "private invokevirtual dispatched to subclass static shadow:\n{combined}"
    );
}
