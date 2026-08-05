//! A real scheduled executor must terminate promptly after its delayed timeout
//! task is cancelled and the executor is shut down. This is the lifecycle used
//! by JUnit 6's same-thread timeout watchdog.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn probe_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/resources/cratonvm/ScheduledExecutorCancelShutdownProbe.java")
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    std::env::var("CRATONVM_BIN")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

#[test]
fn cancelled_delayed_task_does_not_block_scheduled_executor_shutdown() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[scheduled_executor_cancel_shutdown] CRATONVM_BIN unavailable; skipping");
        return;
    };
    let java_home = match std::env::var("CRATONVM_JAVA_HOME") {
        Ok(home) => home,
        Err(_) => {
            eprintln!(
                "[scheduled_executor_cancel_shutdown] CRATONVM_JAVA_HOME unavailable; skipping"
            );
            return;
        }
    };
    let output_dir = std::env::temp_dir().join(format!(
        "cratonvm-scheduled-executor-cancel-shutdown-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&output_dir);
    fs::create_dir_all(&output_dir).expect("create probe output directory");
    let javac = Path::new(&java_home).join("bin/javac");
    let compiled = Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&output_dir)
        .arg(probe_source())
        .output()
        .expect("run javac");
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !compiled.status.success() {
        let stderr_probe = String::from_utf8_lossy(&compiled.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[scheduled_executor_cancel_shutdown_regression] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return;
        }
    }
    assert!(
        compiled.status.success(),
        "[scheduled_executor_cancel_shutdown] the embedded probe failed to compile — \
         fix the probe source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut child = Command::new(binary)
        .args([
            "--java-home",
            &java_home,
            "-cp",
            output_dir.to_str().unwrap(),
        ])
        .arg("cratonvm.ScheduledExecutorCancelShutdownProbe")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scheduled-executor probe");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait().expect("poll scheduled-executor probe") {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("scheduled-executor probe timed out");
            }
        }
    }
    let output = child.wait_with_output().expect("collect probe output");
    let _ = fs::remove_dir_all(&output_dir);
    let combined = format!(
        "{}\n--- STDERR ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "scheduled-executor probe failed:\n{combined}"
    );
    assert!(
        combined.contains("SCHEDULED_CANCEL_SHUTDOWN_OK"),
        "scheduled-executor probe did not finish cleanly:\n{combined}"
    );
}
