// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for `InputStream.read(byte[], int, int)` superclass dispatch.
//!
//! Bouncy Castle's `IndefiniteLengthInputStream.read([BII)` can deliberately
//! call `super.read([BII)` for small buffers. CratonVM's base InputStream native
//! used to redispatch that super-call back to the receiver's override, creating
//! an unbounded cycle and surfacing as `StackOverflowError` in PKCS12Test.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CLASS_NAME: &str = "InputStreamSuperReadProbe20260709";

const SOURCE: &str = r#"
import java.io.ByteArrayInputStream;
import java.io.InputStream;

public class InputStreamSuperReadProbe20260709 {
    static final class IndefiniteLikeInputStream extends InputStream {
        private final InputStream in;
        private int b1;
        private int b2;

        IndefiniteLikeInputStream(byte[] data) throws Exception {
            this.in = new ByteArrayInputStream(data);
            this.b1 = in.read();
            this.b2 = in.read();
        }

        @Override
        public int read() throws java.io.IOException {
            int b = in.read();
            if (b < 0) {
                return -1;
            }
            int v = b1;
            b1 = b2;
            b2 = b;
            return v;
        }

        @Override
        public int read(byte[] buf, int off, int len) throws java.io.IOException {
            return super.read(buf, off, len);
        }
    }

    static final class BulkOverrideInputStream extends InputStream {
        @Override
        public int read() throws java.io.IOException {
            throw new AssertionError("normal read(byte[]) must dispatch to read(byte[],int,int)");
        }

        @Override
        public int read(byte[] buf, int off, int len) {
            buf[off] = 77;
            buf[off + 1] = 88;
            return 2;
        }
    }

    public static void main(String[] args) throws Exception {
        byte[] out = new byte[2];
        int n = new IndefiniteLikeInputStream(new byte[] { 10, 20, 30, 40 }).read(out, 0, out.length);
        int a = out[0] & 0xff;
        int b = out[1] & 0xff;
        if (n != 2 || a != 10 || b != 20) {
            throw new AssertionError("bad read: n=" + n + " a=" + a + " b=" + b);
        }

        byte[] bulk = new byte[2];
        int m = new BulkOverrideInputStream().read(bulk);
        int c = bulk[0] & 0xff;
        int d = bulk[1] & 0xff;
        if (m != 2 || c != 77 || d != 88) {
            throw new AssertionError("bad bulk read: m=" + m + " c=" + c + " d=" + d);
        }

        System.out.println("INPUTSTREAM_SUPER_READ_PASS " + n + " " + a + " " + b + " " + m + " " + c + " " + d);
    }
}
"#;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
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
    let candidate = "C:/Program Files/Java/jdk-25";
    if Path::new(candidate).exists() {
        return Some(candidate.to_string());
    }
    None
}

fn javac_path(java_home: Option<&str>) -> PathBuf {
    if let Some(home) = java_home {
        let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
        let p = Path::new(home).join("bin").join(exe);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(if cfg!(windows) { "javac.exe" } else { "javac" })
}

fn compile_probe(java_home: Option<&str>) -> Option<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "cratonvm-inputstream-super-read-{}-{}",
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
            eprintln!("[inputstream_super_read_probe] javac could not be executed: {e}; skipping");
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
                "[inputstream_super_read_probe] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join(format!("{CLASS_NAME}.class")).exists(),
        "[inputstream_super_read_probe] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn inputstream_super_read_byte_array_does_not_reenter_override() {
    let java_home = java_home();
    let classes = match compile_probe(java_home.as_deref()) {
        Some(c) => c,
        None => {
            eprintln!("[inputstream_super_read_probe] javac unavailable; skipping");
            return;
        }
    };
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[inputstream_super_read_probe] cratonvm binary missing; skipping");
            return;
        }
    };

    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home.as_deref() {
        cmd.arg("--java-home").arg(home);
    }
    cmd.arg("--stack-dump-on-timeout")
        .arg("0")
        .arg("--Xmx")
        .arg("64m")
        .arg("-c")
        .arg(&classes)
        .arg(CLASS_NAME)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let start = Instant::now();
    let timeout = Duration::from_secs(60);
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
    assert_eq!(
        out.status.code(),
        Some(0),
        "rc={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout,
        stderr
    );
    assert!(
        stdout.contains("INPUTSTREAM_SUPER_READ_PASS 2 10 20 2 77 88"),
        "unexpected stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
