// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test: a JIT-compiled method that invokes an instance method on a
//! **null receiver** must throw `NullPointerException`, identically to the
//! interpreter.
//!
//! ## The bug
//!
//! The JIT invoke helpers (`vm/src/jit/helpers.rs`) bailed to `return 0` when
//! the receiver slot was null instead of raising NPE — both the cold
//! `jit_invoke_dispatch` (`match … { _ => return 0 }`) and the monomorphic
//! `jit_invoke_virtual_mic` (`if receiver_raw == 0 { return 0 }`). So a null
//! deref inside JIT-compiled code completed normally rather than throwing. It
//! surfaced most visibly with lambda / method-reference bodies, which the
//! lambda dispatch path eagerly compiles and invokes once: `() -> nullRef.foo()`
//! ran as a silent no-op under the JIT (interpreter-correct, JIT-only bug).
//!
//! ## What this verifies
//!
//! Same shape as `jit_interp_differential.rs`: run the SAME Java program once
//! interpreted (the oracle) and once JIT'd, and assert byte-for-byte identical
//! stdout. The fixture (`cratonvm/JitNull.java`, pure user-defined classes so
//! it runs in synthetic mode with no real JDK) exercises both helper paths:
//!   - `warmed-null`  — a warmed (MIC) callsite invoked on null.
//!   - `cold-null`    — a cold (dispatch) callsite invoked on null.
//! Both must report `NPE` in both engines. Before the fix the JIT engine
//! reported `RET` for both (the swallowed NPE) — a divergence this test fails on.
//!
//! Prereq-gated exactly like the other subprocess tests: skips (does not fail)
//! when `javac` did not compile the fixture or the `cratonvm` binary is absent.
//! Build the binary with `cargo build -p cratonvm-cli` (or set `CRATONVM_BIN`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "JitNull";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

fn classpath_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
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
    let target = workspace_root().join("target");
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

fn class_file_present(simple_name: &str) -> bool {
    classpath_dir()
        .join("cratonvm")
        .join(format!("{simple_name}.class"))
        .exists()
}

#[derive(Clone, Copy)]
enum Mode {
    Interpreter,
    Jit,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Interpreter => "interpreter",
            Mode::Jit => "jit",
        }
    }
    fn apply_env(self, cmd: &mut Command) {
        match self {
            Mode::Interpreter => {
                cmd.env("CRATONVM_DISABLE_JIT", "1");
                cmd.env_remove("CRATONVM_JIT_THRESHOLD");
            }
            Mode::Jit => {
                cmd.env_remove("CRATONVM_DISABLE_JIT");
                // `CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/` was dropped here:
                // `d1979bec5` deleted the static package-ban machinery and its
                // last reader, so the variable no longer reaches anything.
                cmd.env("CRATONVM_JIT_THRESHOLD", "1");
            }
        }
    }
}

fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// Run the fixture under `mode`; `None` if a prerequisite is missing.
fn run(mode: Mode) -> Option<String> {
    if !class_file_present(FIXTURE) {
        eprintln!("[jit_null_recv] {FIXTURE}.class not compiled — skipping.");
        return None;
    }
    let bin = cratonvm_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("-c")
        .arg(classpath_dir())
        .arg(format!("cratonvm.{FIXTURE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    mode.apply_env(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[jit_null_recv] spawn failed ({}): {e}", mode.label());
            return None;
        }
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > RUN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[jit_null_recv] {FIXTURE} ({}) timed out", mode.label());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                eprintln!("[jit_null_recv] try_wait failed: {e}");
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    Some(normalize(&String::from_utf8_lossy(&output.stdout)))
}

/// Pull the `key=value` result lines (drops incidental log noise).
fn result_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.starts_with("warmed-null=") || l.starts_with("cold-null="))
        .collect()
}

#[test]
fn jit_null_receiver_invoke_throws_npe_like_interpreter() {
    let (interp, jit) = match (run(Mode::Interpreter), run(Mode::Jit)) {
        (Some(i), Some(j)) => (i, j),
        _ => {
            eprintln!("[jit_null_recv] prerequisites missing — skipping (not a failure).");
            return;
        }
    };

    let interp_results = result_lines(&interp);
    let jit_results = result_lines(&jit);

    // The interpreter is the oracle: a null-receiver invoke throws NPE.
    assert_eq!(
        interp_results,
        vec!["warmed-null=NPE", "cold-null=NPE"],
        "interpreter oracle changed; full stdout:\n{interp}"
    );

    // The JIT must match the interpreter exactly. Before the fix the JIT
    // swallowed the NPE and reported `RET` (the regression this guards).
    assert_eq!(
        jit_results, interp_results,
        "JIT diverged from interpreter on a null-receiver invoke (NPE swallowed?).\n\
         interpreter stdout:\n{interp}\n\njit stdout:\n{jit}"
    );
}
