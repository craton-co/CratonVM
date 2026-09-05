// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: JVMS §6.5 — `invokevirtual` / `invokespecial` /
//! `invokeinterface` raise `NullPointerException` when `objectref` is null, and
//! must keep doing so once the call site's monomorphic inline cache is warm.
//!
//! `execute_invokevirtual_cached`'s `VirtualBytecode` arm has always deferred a
//! `Value::Object(None)` receiver to the slow path (which owns both the
//! canonical NPE and the deliberate null-tolerant shims). Its `Bytecode` arm —
//! which serves `invokespecial`, i.e. every private/super call — and its
//! `Native` arm never got the same guard: they popped the null straight into
//! `args[0]` and pushed the callee frame (or invoked the registered native)
//! anyway.
//!
//! Measured before the fix: the FIRST `callPrivateOn(null)` throws NPE
//! correctly, and after 50 000 warming calls the SAME site returns `3` — the
//! private method's body, executed with a null `this`. A cold call and a warm
//! call disagreed about whether the program had already failed.
//!
//! That divergence is how a null element in `Class.getPermittedSubclasses0()`
//! surfaced as `NullPointerException: Cannot read field "interfaces" because
//! "rd" is null` at `Class.java:1217` instead of a plain NPE at
//! `Class.isDirectSubType`: `c.getInterfaces(false)` is an invokespecial, the
//! callee frame was pushed with `this == null`, and `Class.reflectionData()`'s
//! registered native answers a null receiver with a null RETURN. See
//! `sealed-derencodable-getinterfaces-npe-mockito-x509-FIXED.md`.
//!
//! The probe deliberately checks the WARM answers — a cold-only test passed
//! throughout the entire lifetime of the bug.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class NullReceiverCachedProbe {
  interface Iface { int ifaceCall(); }

  static class Impl implements Iface {
    public int ifaceCall() { return 1; }
    public int virtualCall() { return 2; }
    private int privateCall() { return 3; }
    // javac emits `invokespecial Impl.privateCall()` here — the same opcode
    // JDK's Class.isDirectSubType uses for `c.getInterfaces(boolean)`.
    static int callPrivateOn(Impl t) { return t.privateCall(); }
    static int callVirtualOn(Impl t) { return t.virtualCall(); }
    static int callIfaceOn(Iface t) { return t.ifaceCall(); }
  }

  static void check(String label, java.util.function.IntSupplier s) {
    try {
      int v = s.getAsInt();
      System.out.println(label + "=NO-THROW(" + v + ")");
    } catch (NullPointerException e) {
      System.out.println(label + "=NPE");
    } catch (Throwable t) {
      System.out.println(label + "=OTHER(" + t.getClass().getName() + ")");
    }
  }

  public static void main(String[] a) {
    // Warm each site to steady state FIRST, so every check below is answered
    // by the inline cache rather than the cold slow path.
    Impl real = new Impl();
    int sink = 0;
    for (int i = 0; i < 50000; i++) {
      sink += Impl.callPrivateOn(real);
      sink += Impl.callVirtualOn(real);
      sink += Impl.callIfaceOn(real);
    }
    if (sink == 0) System.out.println("unreachable");
    check("warm-invokespecial", () -> Impl.callPrivateOn(null));
    check("warm-invokevirtual", () -> Impl.callVirtualOn(null));
    check("warm-invokeinterface", () -> Impl.callIfaceOn(null));
    System.out.println("OK");
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

/// Prerequisite gate: the lookup below is unchanged — only a MISSING JDK is
/// reported differently. See `common::require_jdk`.
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

/// `None` means "no usable `javac` on this machine" — a legitimate skip. A
/// javac that RUNS and rejects the source is a broken probe, not a skip; see
/// the matching comment in `sealed_bootstrap_permitted_subclasses.rs`.
fn compile_probe(javac: &Path) -> Option<PathBuf> {
    if !javac.exists() {
        return None;
    }
    let dir = std::env::temp_dir().join("cratonvm-null-receiver-cached-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("NullReceiverCachedProbe.java");
    let class_file = dir.join("NullReceiverCachedProbe.class");
    let _ = std::fs::remove_file(&class_file);
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[null_receiver_cached] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[null_receiver_cached_invoke] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && class_file.exists(),
        "[null_receiver_cached] the embedded probe failed to compile — fix PROBE_SRC. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// `jit` selects `--nojit` (false) or the default JIT pipeline (true): the
/// interpreter's inline cache and the JIT's own dispatch are separate code
/// paths and both must honour the null check.
fn run_probe(jit: bool) -> Option<String> {
    let bin = cratonvm_binary()?;
    let jdk = jdk_home()?;
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let classes = compile_probe(&javac)?;

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home").arg(&jdk).arg("--Xmx").arg("1g");
    if !jit {
        cmd.arg("--nojit");
    }
    cmd.arg("-cp")
        .arg(&classes)
        .arg("NullReceiverCachedProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().ok()?;
    let timeout = Duration::from_secs(180);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[null_receiver_cached] probe timed out after {timeout:?} (jit={jit})");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[null_receiver_cached] try_wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn assert_all_npe(stdout: &str, jit: bool) {
    assert!(
        stdout.contains("OK"),
        "[null_receiver_cached] probe did not complete (jit={jit}).\nstdout:\n{stdout}"
    );
    for site in [
        "warm-invokespecial",
        "warm-invokevirtual",
        "warm-invokeinterface",
    ] {
        assert!(
            stdout.contains(&format!("{site}=NPE")),
            "[null_receiver_cached] `{site}` on a null receiver did not throw \
             NullPointerException once its inline cache was warm (jit={jit}). A \
             `NO-THROW` here means the callee ran with `this == null` — JVMS §6.5 \
             violation, and the mechanism behind the bogus \
             `Cannot read field \"interfaces\" because \"rd\" is null` at \
             Class.java:1217.\nstdout:\n{stdout}"
        );
    }
}

#[test]
fn warm_null_receiver_invokes_throw_npe_interpreted() {
    let Some(stdout) = run_probe(false) else {
        eprintln!("[null_receiver_cached] cratonvm binary / JDK / javac unavailable; skipping");
        return;
    };
    assert_all_npe(&stdout, false);
}

#[test]
fn warm_null_receiver_invokes_throw_npe_jit() {
    let Some(stdout) = run_probe(true) else {
        eprintln!("[null_receiver_cached] cratonvm binary / JDK / javac unavailable; skipping");
        return;
    };
    assert_all_npe(&stdout, true);
}
