// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 9 wave 6 (`vmhelp6`): a compiled `multianewarray` whose allocation
//! fails must throw a CATCHABLE `OutOfMemoryError`, exactly as `--nojit` does.
//!
//! `jit_multianewarray_2d` used to discard `multianewarray_alloc`'s `Err`
//! (`Err(_) => 0`). That function RETURNS its failure -- the heap-exhaustion
//! arms of `alloc_multi_array` return `InternalError(Runtime(OutOfMemoryError))`
//! -- it does not leave it pending, so the compiled code bailed on the 0
//! sentinel with nothing pending. Measured on `cratonvm-jitr9-w5b.exe`: the
//! warmed `tryAlloc(64, 100_000_000)` below answered `0` (the handler never
//! ran) under the JIT and `-1` under `--nojit`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class R9w6MultiOomProbe {
    static int tryAlloc(int a, int b) {
        try {
            long[][] x = new long[a][b];
            return x.length + x[0].length > 0 ? 1 : 0;
        } catch (OutOfMemoryError e) {
            return -1;
        }
    }

    public static void main(String[] args) {
        long acc = 0;
        for (int i = 0; i < 200000; i++) {
            acc += tryAlloc(2, 3);
        }
        System.out.println("warm=" + acc);
        String huge;
        try {
            huge = Integer.toString(tryAlloc(64, 100_000_000));
        } catch (Throwable t) {
            huge = "escaped:" + t.getClass().getName();
        }
        System.out.println("huge=" + huge);
        System.out.println("small=" + tryAlloc(3, 4));
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

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for cand in [
        "/home/victor/jdk25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-r9w6-vmhelp6-multi-oom-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("R9w6MultiOomProbe.java");
    let _ = std::fs::remove_file(dir.join("R9w6MultiOomProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "[r9w6_vmhelp6_multianewarray_oom] javac could not be executed: {e}; skipping"
            );
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[r9w6_vmhelp6_multianewarray_oom] javac cannot target --release 21; skipping"
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("R9w6MultiOomProbe.class").exists(),
        "[r9w6_vmhelp6_multianewarray_oom] the embedded probe failed to compile. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, nojit: bool) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home").arg(jdk).arg("--Xmx").arg("256m");
    if nojit {
        cmd.arg("--nojit");
    }
    cmd.arg("-c")
        .arg(classes)
        .arg("R9w6MultiOomProbe")
        .env("CRATONVM_DISABLE_DEFAULT_WATCHDOG", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    let timeout = Duration::from_secs(300);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[r9w6_vmhelp6_multianewarray_oom] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("[r9w6_vmhelp6_multianewarray_oom] wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect probe output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn field(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}

#[test]
fn compiled_multianewarray_oom_is_catchable_like_the_interpreter() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[r9w6_vmhelp6_multianewarray_oom] cratonvm binary not found; skipping.");
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[r9w6_vmhelp6_multianewarray_oom] no usable JDK found; skipping.");
        return;
    };
    let javac = jdk.join(if cfg!(windows) {
        "bin/javac.exe"
    } else {
        "bin/javac"
    });
    let Some(classes) = compile_probe(&javac) else {
        return;
    };

    for nojit in [true, false] {
        let (stdout, stderr) = run_probe(&bin, &jdk, &classes, nojit);
        let tier = if nojit { "--nojit" } else { "JIT" };
        assert!(
            stdout.contains("OK"),
            "[{tier}] probe did not reach its final marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert_eq!(
            field(&stdout, "warm=").as_deref(),
            Some("200000"),
            "[{tier}]"
        );
        assert_eq!(
            field(&stdout, "huge=").as_deref(),
            Some("-1"),
            "[{tier}] a failed `new long[64][100_000_000]` must land in the method's own \
             `catch (OutOfMemoryError)`.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert_eq!(field(&stdout, "small=").as_deref(), Some("1"), "[{tier}]");
    }
}
