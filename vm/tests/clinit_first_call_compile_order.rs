// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: the eager first-call JIT compile must not run a `<clinit>`.
//!
//! Entering a compiled artifact runs the `static_init_classes` pre-walk in
//! `interpreter::execute`, which `ensure_class_initialized`s the declaring
//! class of EVERY getstatic/putstatic site anywhere in the body, before
//! bytecode zero. For an ordinary method that approximates JVMS §5.5. For a
//! `<clinit>` it inverts the order the initializer exists to establish.
//!
//! The JDK has an initializer whose correctness is exactly that order:
//! `java/lang/constant/ConstantDescs.<clinit>` assigns `BSM_PRIMITIVE_CLASS`
//! (ConstantDescs.java:198) and only later reads `PrimitiveClassDescImpl.CD_int`
//! (line 249) — while `PrimitiveClassDescImpl.<clinit>`'s own constructor reads
//! `ConstantDescs.BSM_PRIMITIVE_CLASS` back. Hoisting the line-249 trigger to
//! method entry ran that constructor against a null `BSM_PRIMITIVE_CLASS`:
//!
//!   ExceptionInInitializerError
//!     class=jdk/internal/constant/PrimitiveClassDescImpl
//!     cause=java/lang/NullPointerException
//!
//! and `ConstantDescs.<clinit>` then executed **zero** putstatics, poisoning
//! both classes for the rest of the process.
//!
//! `MethodHandles.arrayElementVarHandle(int[].class)` reaches that chain
//! through `VarForm` → `MethodType` → `MethodTypeForm` →
//! `sun/invoke/util/Wrapper.<clinit>`, which is why the probe below is four
//! lines long. It was found as a 100 %-of-runs failure of the documented
//! `CRATONVM_BG_COMPILE=0` opt-out on the Hibernate suite, where it looked
//! like a reproduction of an unrelated JIT crash.
//!
//! Both flag arms are asserted: the default (background) pipeline never took
//! this door, so a green default arm proves nothing on its own.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class ClinitOrderProbe {
    public static void main(String[] args) throws Throwable {
        // Reaches sun.invoke.util.Wrapper.<clinit> -> ConstantDescs.<clinit>
        // -> PrimitiveClassDescImpl.<clinit>, the JDK cycle at stake.
        VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class);
        int[] arr = new int[8];
        vh.set(arr, 3, 42);
        System.out.println("varHandle=" + (int) vh.get(arr, 3));

        // The statics whose assignment the inverted order skipped. A null here
        // is the same defect surviving as a wrong value instead of a throw.
        System.out.println("CD_int=" + java.lang.constant.ConstantDescs.CD_int);
        System.out.println("CD_boolean=" + java.lang.constant.ConstantDescs.CD_boolean);
        System.out.println("BSM_PRIMITIVE_CLASS_null="
            + (java.lang.constant.ConstantDescs.BSM_PRIMITIVE_CLASS == null));
        System.out.println("OK");
    }
}
"#;

mod common;

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

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

/// Probe the install roots rather than pinning a point release that ages out
/// and turns this test into a silent skip.
fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.join("bin").exists() {
                return Some(p);
            }
        }
    }
    for root in [
        "C:/Program Files/Eclipse Adoptium",
        "C:/Program Files/Java",
        "/usr/lib/jvm",
    ] {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("jdk-2") || n.contains("java-2"))
                    && p.join("bin").exists()
            })
            .collect();
        candidates.sort();
        if let Some(best) = candidates.pop() {
            return Some(best);
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-clinit-order-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("ClinitOrderProbe.java");
    let _ = std::fs::remove_file(dir.join("ClinitOrderProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[clinit_order] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!("[clinit_order] javac cannot target --release 21; skipping");
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("ClinitOrderProbe.class").exists(),
        "[clinit_order] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, bg_compile_off: bool) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home").arg(jdk);
    if bg_compile_off {
        cmd.env("CRATONVM_BG_COMPILE", "0");
    } else {
        cmd.env_remove("CRATONVM_BG_COMPILE");
    }
    cmd.arg("-cp")
        .arg(classes)
        .arg("ClinitOrderProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    let timeout = Duration::from_secs(180);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[clinit_order] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[clinit_order] try_wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect output");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn assert_clinit_order(stdout: &str, stderr: &str, arm: &str) {
    let line = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| {
                panic!(
                    "[clinit_order/{arm}] missing `{key}` line — the JDK constant-descriptor \
                     <clinit> cycle did not complete.\nstdout:\n{stdout}\nstderr:\n{stderr}"
                )
            })
            .to_string()
    };
    assert_eq!(line("varHandle"), "42", "[{arm}] VarHandle round-trip");
    // Not just non-null: the descriptor text proves the constructor ran with a
    // real `BSM_PRIMITIVE_CLASS`, which is the assignment the inverted order
    // skipped.
    assert_eq!(
        line("CD_int"),
        "PrimitiveClassDesc[int]",
        "[{arm}] ConstantDescs.CD_int"
    );
    assert_eq!(
        line("CD_boolean"),
        "PrimitiveClassDesc[boolean]",
        "[{arm}] ConstantDescs.CD_boolean"
    );
    assert_eq!(
        line("BSM_PRIMITIVE_CLASS_null"),
        "false",
        "[{arm}] ConstantDescs.BSM_PRIMITIVE_CLASS must be assigned"
    );
    assert!(
        !stderr.contains("ExceptionInInitializerError"),
        "[{arm}] a class initializer failed.\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("OK"),
        "[{arm}] probe did not finish.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn a_clinit_is_not_first_call_compiled_in_either_bg_compile_arm() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[clinit_order] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`. Skipping."
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!("[clinit_order] no JDK home (set CRATONVM_TEST_JDK or JAVA_HOME); skipping");
            return;
        }
    };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let classes = match compile_probe(&javac) {
        Some(d) => d,
        None => return,
    };

    // `bg-compile=0` is the arm that regressed; the default arm is asserted too
    // so a future change that moves the first-call door under the background
    // pipeline cannot reintroduce this silently.
    for (arm, bg_off) in [("bg-compile=0", true), ("default", false)] {
        let (stdout, stderr) = run_probe(&bin, &jdk, &classes, bg_off);
        assert_clinit_order(&stdout, &stderr, arm);
    }
}
