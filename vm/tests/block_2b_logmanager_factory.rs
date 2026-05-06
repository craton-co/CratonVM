//! Block 2B — `java.util.logging.LogManager.getLogManager()` honours the
//! `-Djava.util.logging.manager=<className>` system property.
//!
//! The `apps/lm_subclass/LmSubclass` probe exercises both:
//!   * the default case (no property set) —
//!     `getLogManager().getClass().getName()` is `java.util.logging.LogManager`,
//!   * the property-driven case
//!     (`-Djava.util.logging.manager=LmSubclass$MyLm`) —
//!     `MyLm-init` is printed by the subclass ctor and the same
//!     `getClass().getName()` returns `LmSubclass$MyLm`.
//!
//! The probe is run as a subprocess against the rustjvm CLI binary.
//! Skips cleanly with a diagnostic if the binary, the compiled class
//! files, or the real JDK are unavailable so CI doesn't hard-fail when
//! the worktree hasn't been built or when JDK 25 isn't installed at the
//! expected path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn probe_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().join("apps").join("lm_subclass")
}

fn rustjvm_binary() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("RUSTJVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
    let exe = if cfg!(windows) { "rustjvm.exe" } else { "rustjvm" };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let cls = dir.join("LmSubclass.class");
    let inner = dir.join("LmSubclass$MyLm.class");
    if cls.exists() && inner.exists() {
        return true;
    }
    let src = dir.join("LmSubclass.java");
    if !src.exists() {
        return false;
    }
    let status = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&src)
        .status();
    matches!(status, Ok(s) if s.success())
        && cls.exists()
        && inner.exists()
}

fn jdk_home() -> Option<PathBuf> {
    if let Ok(j) = std::env::var("RUSTJVM_TEST_JDK") {
        let p = PathBuf::from(&j);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(j) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&j);
        if p.exists() {
            return Some(p);
        }
    }
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default.exists() {
        return Some(default);
    }
    None
}

/// Run the LmSubclass probe with the given extra args (e.g. a -D
/// property override) and capture stdout+stderr. Returns
/// `Some((stdout, stderr, success))` or `None` if a prerequisite is
/// missing (test should skip).
fn run_probe(extra_args: &[&str]) -> Option<(String, String, bool)> {
    if !ensure_probe_compiled() {
        eprintln!("[block_2b] LmSubclass.class unavailable; skipping");
        return None;
    }
    let bin = match rustjvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[block_2b] rustjvm binary not found; build with \
                 `cargo build --release -p rustjvm-cli`"
            );
            return None;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[block_2b] no JDK home (set RUSTJVM_TEST_JDK or JAVA_HOME); skipping"
            );
            return None;
        }
    };
    let classes = probe_dir();
    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home").arg(&jdk);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg("-c")
        .arg(&classes)
        .arg("LmSubclass")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[block_2b] failed to spawn rustjvm: {e}");
            return None;
        }
    };
    let timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[block_2b] LmSubclass timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[block_2b] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    Some((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    ))
}

#[test]
fn block_2b_default_log_manager_when_property_unset() {
    let Some((stdout, stderr, success)) = run_probe(&[]) else {
        return;
    };
    assert!(
        stdout.contains("class=java.util.logging.LogManager"),
        "expected default class line.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "expected OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("MyLm-init"),
        "MyLm-init must NOT print without -Djava.util.logging.manager.\nstdout:\n{stdout}"
    );
    assert!(
        success,
        "default-case probe must exit zero.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn block_2b_subclass_log_manager_honours_system_property() {
    let Some((stdout, stderr, success)) = run_probe(&["-Djava.util.logging.manager=LmSubclass$MyLm"]) else {
        return;
    };
    assert!(
        stdout.contains("MyLm-init"),
        "subclass ctor must run (MyLm-init line absent).\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("class=LmSubclass$MyLm"),
        "getClass().getName() must reflect the subclass.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "expected OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        success,
        "subclass-case probe must exit zero.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
