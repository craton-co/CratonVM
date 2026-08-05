// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for real-JDK `Writer.flush()` dispatch.
//!
//! Spring's `MockHttpServletResponse.ResponsePrintWriter` flushes its wrapped
//! `OutputStreamWriter` after every write, then `getContentAsString()` reads the
//! backing `ByteArrayOutputStream` before the writer is closed. A base-class
//! `java/io/Writer.flush()V` no-op native used to shadow the real
//! `OutputStreamWriter.flush()` bytecode, leaving `StreamEncoder` bytes pending
//! and making the response body read back as empty.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CLASS_NAME: &str = "WriterFlushRealProbe20260709";

const SOURCE: &str = r#"
import java.io.ByteArrayOutputStream;
import java.io.OutputStreamWriter;
import java.io.PrintWriter;
import java.io.Writer;
import java.nio.charset.StandardCharsets;

public class WriterFlushRealProbe20260709 {
    static final class FlushAfterWritePrintWriter extends PrintWriter {
        FlushAfterWritePrintWriter(Writer out) {
            super(out, true);
        }

        @Override
        public void write(String s, int off, int len) {
            super.write(s, off, len);
            super.flush();
        }

        @Override
        public void write(char[] buf, int off, int len) {
            super.write(buf, off, len);
            super.flush();
        }

        @Override
        public void write(int c) {
            super.write(c);
            super.flush();
        }
    }

    private static void check(String value) throws Exception {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        FlushAfterWritePrintWriter pw =
                new FlushAfterWritePrintWriter(new OutputStreamWriter(baos, StandardCharsets.UTF_8));
        pw.write(value, 0, value.length());
        String actual = baos.toString(StandardCharsets.UTF_8);
        if (!value.equals(actual)) {
            throw new AssertionError("after write+flush expected [" + value + "] but got [" + actual + "]");
        }
    }

    public static void main(String[] args) throws Exception {
        check("X");
        check("{\"name\":\"J\\u00FCrgen\"}");
        System.out.println("WRITER_FLUSH_REAL_PASS");
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
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
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

fn java_home() -> Option<PathBuf> {
    for var in ["CRATONVM_TEST_JDK", "CRATONVM_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(var) {
            let p = PathBuf::from(home);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for candidate in [
        "C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn javac_path(java_home: Option<&Path>) -> PathBuf {
    if let Some(home) = java_home {
        let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
        let p = home.join("bin").join(exe);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(if cfg!(windows) { "javac.exe" } else { "javac" })
}

fn compile_probe(java_home: Option<&Path>) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "cratonvm-writer-flush-real-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;

    let src = dir.join(format!("{CLASS_NAME}.java"));
    std::fs::write(&src, SOURCE).expect("write probe source");
    let out = match Command::new(javac_path(java_home))
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[writer_flush_real] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // probe source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[writer_flush_real] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join(format!("{CLASS_NAME}.class")).exists(),
        "[writer_flush_real] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn real_output_stream_writer_flush_reaches_backing_stream() {
    let java_home = java_home();
    let classes = match compile_probe(java_home.as_deref()) {
        Some(classes) => classes,
        None => {
            eprintln!("[writer_flush_real] javac unavailable; skipping");
            return;
        }
    };
    let bin = match cratonvm_binary() {
        Some(bin) => bin,
        None => {
            eprintln!("[writer_flush_real] cratonvm binary missing; skipping");
            return;
        }
    };

    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home.as_deref() {
        cmd.arg("--java-home").arg(home);
    }
    cmd.arg("--stack-dump-on-timeout")
        .arg("0")
        .arg("-Xmx64m")
        .arg("-c")
        .arg(&classes)
        .arg(CLASS_NAME)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let timeout = Duration::from_secs(90);
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{CLASS_NAME} timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    let out = child.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{CLASS_NAME} exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(
        stdout.contains("WRITER_FLUSH_REAL_PASS"),
        "unexpected stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
