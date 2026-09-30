// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for cursor divergence between FilterInputStream.skip(long) and
//! the read methods of wrapped real-JDK streams.

use std::path::{Path, PathBuf};
use std::process::Command;

const CLASS_NAME: &str = "FilterInputStreamSkipCursorProbe";

const SOURCE: &str = r#"
import java.io.BufferedInputStream;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.InputStream;

public class FilterInputStreamSkipCursorProbe {
    private static void check(InputStream source, String label, long skip) throws Exception {
        DataInputStream input = new DataInputStream(source);
        for (int i = 0; i < 11; i++) {
            if (input.readUnsignedByte() != i) throw new AssertionError(label + " pre " + i);
        }
        long skipped = input.skip(skip);
        int next = input.readUnsignedByte();
        int expected = (int) ((11L + skip) & 0xffL);
        if (skipped != skip || next != expected) {
            throw new AssertionError(label + " skipped=" + skipped + " next=" + next);
        }
    }

    public static void main(String[] args) throws Exception {
        byte[] bytes = new byte[10_000];
        for (int i = 0; i < bytes.length; i++) bytes[i] = (byte) i;
        check(new ByteArrayInputStream(bytes), "raw-small", 4L);
        check(new BufferedInputStream(new ByteArrayInputStream(bytes)), "buffered-small", 4L);
        check(new ByteArrayInputStream(bytes), "raw-bulk", 7_000L);
        check(new BufferedInputStream(new ByteArrayInputStream(bytes)), "buffered-bulk", 7_000L);
        System.out.println("FILTER_INPUT_STREAM_SKIP_CURSOR_PASS");
    }
}
"#;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn java_home() -> Option<String> {
    std::env::var("CRATONVM_JAVA_HOME")
        .ok()
        .or_else(|| std::env::var("JAVA_HOME").ok())
        .or_else(|| {
            let candidate = "C:/Program Files/Java/jdk-25";
            Path::new(candidate).exists().then(|| candidate.to_string())
        })
}

fn compile_probe(java_home: Option<&str>) -> Option<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("cratonvm-fis-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join(format!("{CLASS_NAME}.java"));
    std::fs::write(&source, SOURCE).expect("write probe source");
    let javac = java_home
        .map(|home| {
            Path::new(home)
                .join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
        })
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "javac.exe" } else { "javac" }));
    let out = match Command::new(javac)
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!(
                "[filter_input_stream_skip_cursor] javac could not be executed: {e}; skipping"
            );
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
                "[filter_input_stream_skip_cursor] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the source: the probe is broken, and skipping here
    // would make this test a permanent vacuous pass.
    assert!(
        out.status.success(),
        "[filter_input_stream_skip_cursor] the embedded probe failed to compile — fix \
         the probe source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn filter_input_stream_skip_and_read_share_one_cursor() {
    let Some(classes) = compile_probe(java_home().as_deref()) else {
        eprintln!("[filter_input_stream_skip_cursor] javac unavailable; skipping");
        return;
    };
    let binary = std::env::var("CRATONVM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            manifest_dir()
                .parent()
                .unwrap()
                .join("target")
                .join(if cfg!(windows) {
                    "debug/cratonvm.exe"
                } else {
                    "debug/cratonvm"
                })
        });
    if !binary.exists() {
        eprintln!("[filter_input_stream_skip_cursor] cratonvm binary missing; skipping");
        return;
    }
    let mut command = Command::new(binary);
    if let Some(home) = java_home() {
        command.arg("--java-home").arg(home);
    }
    let output = command
        .arg("--nojit")
        .arg("-c")
        .arg(classes)
        .arg(CLASS_NAME)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("FILTER_INPUT_STREAM_SKIP_CURSOR_PASS"));
}
