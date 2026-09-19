// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 9 wave 9, lane `cha9`: CHA and the receiver-guarded inline must never
//! bind an `invokeinterface` of a PRIVATE interface method to the receiver
//! class's unrelated public method of the same name.
//!
//! JVMS 11 §6.5 `invokeinterface`, selection step 1: when the resolved method
//! is private it IS the selected method. On w8b the C1 compile of `I.callM`
//! asked `resolve_unique_concrete_bind(I, "m", "()I")`, CHA answered `X`, and
//! the guarded splice resolved `X.m` from the receiver, so the compiled body
//! returned 100 instead of 1 (a few hundred wrong answers per run until the C2
//! body, which resolves from the constant pool, replaced it). `--nojit` was
//! clean. After `cha9`'s CHA/inline guards (w9) one bad call remained: the C1
//! body's interface inline cache (`jit_invoke_virtual_mic`) selected `X.m`
//! from the receiver. Lane `cha9b` pins a private `0xb9` site as a direct bind
//! (`invoke_kind` 1) in every compile door, so it gets no MIC/PIC. See
//! `docs/internal/fixed-bugs/cha-binds-a-private-interface-method-to-the-receivers-public-namesake-FIXED-20260918.md`.
//!
//! Runs as a child `cratonvm` process on a `javac`-compiled probe, in the style
//! of `r9w9_review9b_door_synchronized.rs`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

const PROBE_SRC: &str = r#"
public class PrivIfaceProbe {
    interface I {
        private int m() { return 1; }
        default int callM() { return m(); }   // javac: invokeinterface I.m()I
    }
    // X.m does NOT override the private I.m.
    static class X implements I { public int m() { return 100; } }

    public static void main(String[] args) {
        I x = new X();
        int bad = 0;
        for (int round = 0; round < 5; round++) {
            for (int i = 0; i < 200_000; i++) if (x.callM() != 1) bad++;
        }
        System.out.println("PRIV bad=" + bad + " end");
    }
}
"#;

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

/// Compile the probe into a fresh temp dir; `None` only when `javac` cannot be
/// launched at all.
fn compile_probe() -> Option<tempfile::TempDir> {
    let temp = tempfile::tempdir().expect("temp probe dir");
    let source = temp.path().join("PrivIfaceProbe.java");
    std::fs::File::create(&source)
        .and_then(|mut f| f.write_all(PROBE_SRC.trim_start().as_bytes()))
        .expect("write probe source");
    match Command::new(javac_bin())
        .arg("-d")
        .arg(temp.path())
        .arg(&source)
        .output()
    {
        Err(e) => {
            eprintln!("[r9w9_cha9] javac could not be executed: {e}; skipping");
            None
        }
        Ok(o) => {
            assert!(
                o.status.success(),
                "[r9w9_cha9] the embedded probe failed to compile — fix the probe source. \
                 javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            Some(temp)
        }
    }
}

fn run_probe(bin: &Path, dir: &Path) -> String {
    let mut child = Command::new(bin)
        .arg("-cp")
        .arg(dir)
        .arg("PrivIfaceProbe")
        .env("CRATONVM_DISABLE_DEFAULT_WATCHDOG", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cratonvm");
    let timeout = Duration::from_secs(180);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("PrivIfaceProbe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("probe output");
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A private interface method invoked through `invokeinterface` from a default
/// method must run itself, never the receiver's public namesake, in compiled
/// code as in the interpreter.
#[test]
fn a_private_interface_method_is_never_bound_to_the_receivers_namesake() {
    let Some(bin) = cratonvm_binary() else {
        return;
    };
    let Some(dir) = compile_probe() else {
        return;
    };
    let out = run_probe(&bin, dir.path());
    assert!(
        out.contains("PRIV bad=0 end"),
        "compiled code called X.m() (returns 100) for an invokeinterface of the \
         private I.m() (returns 1) — CHA or the guarded inline selected from the \
         receiver:\n{out}"
    );
}
