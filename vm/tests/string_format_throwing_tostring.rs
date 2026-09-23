// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for `%s` formatting through an anonymous `toString()` override.
//!
//! `String.format` and `String.formatted` must perform virtual dispatch and
//! propagate a Java exception thrown by `toString()`.  Spring Boot's MockMvc
//! result printer relies on that exception reaching its own recovery handler.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SOURCE: &str = r#"
public class StringFormatThrowingToString {
    private static final Object VALUE = new Object() {
        @Override
        public String toString() {
            throw new IllegalStateException("Formatting failed");
        }
    };

    public static void main(String[] args) {
        expectThrow("direct", () -> VALUE.toString());
        expectThrow("formatted", () -> "%s".formatted(VALUE));
        expectThrow("format", () -> String.format("%s", VALUE));
        System.out.println("STRING_FORMAT_THROWING_TOSTRING_OK");
    }

    private static void expectThrow(String operation, Runnable action) {
        try {
            action.run();
            throw new AssertionError(operation + " did not call the anonymous toString override");
        }
        catch (IllegalStateException ex) {
            if (!"Formatting failed".equals(ex.getMessage())) {
                throw new AssertionError(operation + " threw the wrong exception", ex);
            }
            System.out.println(operation + "_THREW");
        }
    }
}
"#;

fn java_home() -> Option<PathBuf> {
    ["CRATONVM_TEST_JDK", "JAVA_HOME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(PathBuf::from)
        .find(|path| path.join("bin").is_dir())
}

fn binary() -> Option<PathBuf> {
    std::env::var("CRATONVM_BIN")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

fn compile_probe(jdk: &Path) -> Option<tempfile::TempDir> {
    let temp = tempfile::tempdir().ok()?;
    let source = temp.path().join("StringFormatThrowingToString.java");
    std::fs::write(&source, SOURCE).expect("write probe source");
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let out = match Command::new(javac)
        .arg("-d")
        .arg(temp.path())
        .arg(&source)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!(
                "[string_format_throwing_tostring] javac could not be executed: {e}; skipping"
            );
            return None;
        }
    };
    {
        assert!(
            out.status.success(),
            "[string_format_throwing_tostring] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(temp)
    }
}

fn run_mode(bin: &Path, jdk: &Path, classes: &Path, no_jit: bool) -> (String, String, bool) {
    let mut command = Command::new(bin);
    command
        .arg("--java-home")
        .arg(jdk)
        .args(no_jit.then_some("--nojit"))
        .arg("-c")
        .arg(classes)
        .arg("StringFormatThrowingToString")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn CratonVM probe");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("poll CratonVM probe") {
            Some(_) => break,
            None if start.elapsed() <= Duration::from_secs(120) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("String-format throwing-toString probe timed out");
            }
        }
    }
    let output = child
        .wait_with_output()
        .expect("collect CratonVM probe output");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

#[test]
fn string_format_propagates_anonymous_tostring_exceptions_in_jit_and_interpreter() {
    let Some(jdk) = java_home() else {
        eprintln!("[string_format_throwing_tostring] JDK unavailable; skipping");
        return;
    };
    let Some(bin) = binary() else {
        eprintln!("[string_format_throwing_tostring] CRATONVM_BIN unavailable; skipping");
        return;
    };
    let Some(temp) = compile_probe(&jdk) else {
        eprintln!("[string_format_throwing_tostring] javac failed; skipping");
        return;
    };

    for no_jit in [false, true] {
        let (stdout, stderr, success) = run_mode(&bin, &jdk, temp.path(), no_jit);
        assert!(
            success,
            "probe exited non-zero (nojit={no_jit}).\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(stdout.contains("formatted_THREW"), "String.formatted swallowed or mis-dispatched toString (nojit={no_jit}).\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("format_THREW"), "String.format swallowed or mis-dispatched toString (nojit={no_jit}).\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("STRING_FORMAT_THROWING_TOSTRING_OK"), "probe did not reach completion (nojit={no_jit}).\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
}
