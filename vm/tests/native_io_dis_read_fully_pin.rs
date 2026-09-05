// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for stale native ObjectRefs in DataInputStream.readFully.
//!
//! The native readFully helper calls the wrapped InputStream via virtual
//! dispatch, then writes into the caller's byte[] after the call returns. A
//! moving GC during the wrapped read can relocate both the DataInputStream and
//! destination byte[]; the helper must pin and re-read those ObjectRefs before
//! the post-call set_array_element.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CLASS_NAME: &str = "DisReadFullyPinProbe";

const SOURCE: &str = r#"
import java.io.DataInputStream;
import java.io.InputStream;

public class DisReadFullyPinProbe {
    static final class ZeroBulkAllocatingStream extends InputStream {
        private int next = 1;
        private int remaining;

        ZeroBulkAllocatingStream(int remaining) {
            this.remaining = remaining;
        }

        @Override
        public int read(byte[] b, int off, int len) {
            return 0;
        }

        @Override
        public int read() {
            byte[][] churn = new byte[4][];
            for (int i = 0; i < churn.length; i++) {
                churn[i] = new byte[1024];
            }
            System.gc();
            if (remaining-- <= 0) {
                return -1;
            }
            return next++ & 0xff;
        }
    }

    public static void main(String[] args) throws Exception {
        int n = 256;
        byte[] out = new byte[n];
        new DataInputStream(new ZeroBulkAllocatingStream(n)).readFully(out);

        long sum = 0;
        for (byte b : out) {
            sum += b & 0xff;
        }
        if (sum != 32640L) {
            throw new AssertionError("bad checksum: " + sum);
        }
        System.out.println("DIS_READ_FULLY_PIN_PASS " + sum);
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
        "cratonvm-dis-readfully-pin-{}-{}",
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
            eprintln!("[native_io_dis_read_fully_pin] javac could not be executed: {e}; skipping");
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
                "[native_io_dis_read_fully_pin] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join(format!("{CLASS_NAME}.class")).exists(),
        "[native_io_dis_read_fully_pin] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn data_input_stream_read_fully_reloads_pinned_byte_array() {
    let java_home = java_home();
    let classes = match compile_probe(java_home.as_deref()) {
        Some(c) => c,
        None => {
            eprintln!("[native_io_dis_read_fully_pin] javac unavailable; skipping");
            return;
        }
    };
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[native_io_dis_read_fully_pin] cratonvm binary missing; skipping");
            return;
        }
    };

    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home.as_deref() {
        cmd.arg("--java-home").arg(home);
    }
    cmd.arg("--Xmx")
        .arg("64m")
        .arg("-c")
        .arg(&classes)
        .arg(CLASS_NAME)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd.spawn().expect("spawn cratonvm");
    // DRAIN THE PIPES WHILE WAITING. A `try_wait` poll loop over piped stdio
    // deadlocks the moment the child outruns the pipe: it blocks in `write`,
    // never exits, and the loop reports a TIMEOUT for a process that finished
    // its work in seconds.
    //
    // Which is what happened here, deterministically, on LINUX only:
    // 10 runs, 10 timeouts at 90 s, while the identical command run from a
    // shell passes in seconds. The release binary emits one `[GC] zgc-pause`
    // line per cycle and this probe drives 256 of them — 92 210 bytes of
    // stderr against a 65 536-byte pipe. The debug binary writes 150 bytes,
    // which is why the same test is green on a debug build and why chasing
    // this with the wrong binary reads as "the probe is fine".
    //
    // `common::wait_draining` exists for exactly this and carries its own
    // regression test (`wait_draining_survives_a_child_that_outruns_the_pipe`).
    // Its doc states the rule this harness broke: a test must not depend on the
    // process it drives staying under 64 KiB.
    let timed = common::wait_draining(child, Duration::from_secs(90));
    let out = timed.output;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !timed.timed_out,
        "DisReadFullyPinProbe timed out after 90s
stdout:
{stdout}
stderr:
{stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "rc={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout,
        stderr
    );
    assert!(
        stdout.contains("DIS_READ_FULLY_PIN_PASS 32640"),
        "unexpected stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
