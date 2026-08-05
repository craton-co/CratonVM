// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for DataInputStream reads on a stream shared with another owner.
//!
//! ObjectInputStream uses an internal DataInputStream over its block-data
//! stream for primitive reads, then reads the same block-data stream directly.
//! A native 8 KiB prefetch used to hide the trailing values in a Rust side
//! buffer, so `readInt()` succeeded and the following `readBoolean()` saw EOF.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CLASS_NAME: &str = "DisSharedStreamProbe20260713";

const SOURCE: &str = r#"
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;

public class DisSharedStreamProbe20260713 {
    public static void main(String[] args) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeInt(0x12345678);
            out.writeBoolean(true);
            out.writeBoolean(false);
            out.writeBoolean(true);
        }

        try (ObjectInputStream in =
                new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            int number = in.readInt();
            boolean first = in.readBoolean();
            boolean second = in.readBoolean();
            boolean third = in.readBoolean();
            if (number != 0x12345678 || !first || second || !third) {
                throw new AssertionError(
                        "bad values: " + number + "," + first + "," + second + "," + third);
            }
        }
        System.out.println("DIS_SHARED_STREAM_PASS");
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
        let path = PathBuf::from(bin);
        if path.exists() {
            return Some(path);
        }
    }
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    let executable = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in ["release", "debug"] {
        let candidate = target.join(profile).join(executable);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn java_home() -> Option<PathBuf> {
    for key in ["CRATONVM_TEST_JDK", "CRATONVM_JAVA_HOME", "JAVA_HOME"] {
        if let Some(home) = std::env::var_os(key).map(PathBuf::from) {
            if home.exists() {
                return Some(home);
            }
        }
    }
    None
}

fn javac_path(java_home: Option<&Path>) -> PathBuf {
    if let Some(home) = java_home {
        let executable = if cfg!(windows) { "javac.exe" } else { "javac" };
        let candidate = home.join("bin").join(executable);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(if cfg!(windows) { "javac.exe" } else { "javac" })
}

fn compile_probe(java_home: Option<&Path>) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "cratonvm-dis-shared-stream-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join(format!("{CLASS_NAME}.java"));
    std::fs::write(&source, SOURCE).expect("write probe source");
    let out = match Command::new(javac_path(java_home))
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
            eprintln!("[native_io_dis_shared_stream] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    {
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
                    "[native_io_dis_shared_stream] javac cannot target --release 21 ({}); skipping. Point \
                     JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                    stderr_probe.lines().next().unwrap_or("").trim()
                );
                return None;
            }
        }
        assert!(
            out.status.success(),
            "[native_io_dis_shared_stream] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(dir)
    }
}

#[test]
fn data_input_stream_does_not_prefetch_from_shared_object_stream() {
    let java_home = java_home();
    let Some(classes) = compile_probe(java_home.as_deref()) else {
        eprintln!("javac unavailable; skipping shared-stream regression");
        return;
    };
    let Some(binary) = cratonvm_binary() else {
        eprintln!("cratonvm binary unavailable; skipping shared-stream regression");
        return;
    };

    let mut command = Command::new(binary);
    if let Some(home) = java_home.as_deref() {
        command.arg("--java-home").arg(home);
    }
    command
        .arg("--Xmx")
        .arg("64m")
        .arg("-c")
        .arg(&classes)
        .arg(CLASS_NAME)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command.spawn().expect("spawn cratonvm");
    let start = Instant::now();
    let timeout = Duration::from_secs(90);
    loop {
        match child.try_wait().expect("poll cratonvm") {
            Some(_) => break,
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{CLASS_NAME} timed out");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    let output = child.wait_with_output().expect("collect cratonvm output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{CLASS_NAME} failed: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("DIS_SHARED_STREAM_PASS"),
        "missing success marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
