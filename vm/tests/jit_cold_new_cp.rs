// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: a hot method whose only allocation is a `new`/`anewarray` of a
//! NOT-YET-LOADED class must still compile, and the compiled site must resolve
//! that class correctly the first time the cold branch is finally taken.
//!
//! Before the fix (`jit-compile-bail-unresolved-new-cold-class.md`)
//! the compile-time resolver returned "unresolvable" for such a site, which
//! bailed the WHOLE compile; after `MAX_TIER_FAIL_RETRIES` the method was never
//! retried and interpreted forever. json-smart's `JSONParserBase.readMain`
//! accumulated 293,940 interpreted invocations that way, purely because nothing
//! had loaded `ParseException`.
//!
//! The fix compiles the site to a constant-pool-indexed helper
//! (`jit_new_object_cp` / `jit_anewarray_object_cp`) that resolves at run time.
//! This test covers the RUNTIME half — the jit-crate's
//! `deferred_new_site_compiles_when_cp_helper_is_wired` covers the compile half:
//!
//!   * the hot method is not reported as a compile failure, AND
//!   * the cold `new`, once taken, produces the right exception object and runs
//!     the target's `<clinit>` at that point (not earlier), AND
//!   * the cold `anewarray` produces a correctly-typed, correctly-sized array.
//!
//! The `<clinit>`-timing assertions are what make this non-vacuous: they fail
//! both if the class was loaded too early (site was never deferred — the test
//! would be testing nothing) and if the helper skipped initialisation.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class ColdNewCpProbe {
    // Written by Cold's <clinit>. Stays false until the cold branch is taken.
    static boolean coldClinitRan = false;

    // Never loaded while `hot` is warming up: nothing else mentions it, and the
    // catch below is on RuntimeException so the exception table does not name it
    // either.
    static final class Cold extends RuntimeException {
        static { ColdNewCpProbe.coldClinitRan = true; }
        Cold(String m) { super(m); }
    }

    // Component class of the cold `anewarray`. Same story.
    static final class Elem {
        int v;
    }

    static int hot(int x) {
        if (x < 0) {
            throw new Cold("cold-taken");
        }
        int s = x;
        for (int i = 0; i < 16; i++) {
            s = s * 31 + (x ^ i);
        }
        return s;
    }

    static Object hotArr(int x) {
        if (x < 0) {
            return new Elem[3];
        }
        return null;
    }

    public static void main(String[] args) {
        int acc = 0;
        for (int i = 0; i < 60000; i++) {
            acc += hot(i);
            hotArr(i);
        }
        System.out.println("acc=" + acc);
        System.out.println("clinitBeforeCold=" + coldClinitRan);

        String msg = "none";
        String cls = "none";
        try {
            hot(-5);
        } catch (RuntimeException e) {
            msg = String.valueOf(e.getMessage());
            cls = e.getClass().getName();
        }
        System.out.println("coldMsg=" + msg);
        System.out.println("coldClass=" + cls);
        System.out.println("clinitAfterCold=" + coldClinitRan);

        Object arr = hotArr(-1);
        System.out.println("arrClass=" + (arr == null ? "null" : arr.getClass().getName()));
        System.out.println("arrLen=" + (arr instanceof Object[] ? ((Object[]) arr).length : -1));
        System.out.println("arrElem0Null=" + (arr instanceof Object[] && ((Object[]) arr)[0] == null));

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
    let dir = std::env::temp_dir().join("cratonvm-jit-cold-new-cp-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("ColdNewCpProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("ColdNewCpProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[jit_cold_new_cp] javac could not be executed: {e}; skipping");
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
                "[jit_cold_new_cp] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("ColdNewCpProbe.class").exists(),
        "[jit_cold_new_cp] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // Surfaces the per-method tiering table on stderr, including the
        // "hot method(s) whose COMPILE FAILED" list this test reads.
        .env("CRATONVM_DBG", "jit-method-stats")
        .arg("-c")
        .arg(classes)
        .arg("ColdNewCpProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
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
                    panic!("[jit_cold_new_cp] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[jit_cold_new_cp] try_wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect output");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn hot_method_with_a_cold_new_compiles_and_resolves_at_runtime() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[jit_cold_new_cp] cratonvm binary not found; build with \
                 `cargo build --release -p cratonvm-cli`. Skipping."
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[jit_cold_new_cp] no JDK home (set CRATONVM_TEST_JDK or \
                 JAVA_HOME); skipping"
            );
            return;
        }
    };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let classes = match compile_probe(&javac) {
        Some(d) => d,
        None => {
            eprintln!("[jit_cold_new_cp] javac unavailable; skipping");
            return;
        }
    };

    let (stdout, stderr) = run_probe(&bin, &jdk, &classes);
    let line = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| {
                panic!(
                    "[jit_cold_new_cp] missing `{key}` line.\n\
                     stdout:\n{stdout}\nstderr:\n{stderr}"
                )
            })
            .to_string()
    };

    assert!(
        stdout.contains("OK"),
        "[jit_cold_new_cp] probe did not finish.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // (1) The point of the whole fix: the hot method must not be a permanent
    //     compile failure. The banner assertion keeps this honest — if the
    //     diagnostic ever stops being emitted, this fails loudly instead of
    //     silently passing on an empty haystack.
    assert!(
        stderr.contains("JIT method stats:"),
        "[jit_cold_new_cp] CRATONVM_DBG=jit-method-stats produced no stats \
         banner; the compile-failure assertion below would be vacuous.\n\
         stderr:\n{stderr}"
    );
    let stuck: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("compile-failed") && l.contains("ColdNewCpProbe.hot"))
        .collect();
    assert!(
        stuck.is_empty(),
        "[jit_cold_new_cp] a hot method containing only a COLD `new` of a \
         not-yet-loaded class must still compile; it was reported as a \
         permanent compile failure:\n{stuck:#?}"
    );

    // (2) The site was genuinely deferred: `Cold` was NOT loaded while the
    //     method was warming up. If this is `true`, something loaded the class
    //     early and assertion (1) proves nothing about the deferred path.
    assert_eq!(
        line("clinitBeforeCold="),
        "false",
        "[jit_cold_new_cp] `Cold` must still be unloaded after the warm-up \
         loop — otherwise the `new` site was never deferred and this test is \
         not exercising the fix"
    );

    // (3) The deferred `new` resolves, initialises and allocates correctly when
    //     the cold branch is finally taken.
    assert_eq!(
        line("coldClass="),
        "ColdNewCpProbe$Cold",
        "[jit_cold_new_cp] the deferred `new` must allocate the class the \
         bytecode names"
    );
    assert_eq!(
        line("coldMsg="),
        "cold-taken",
        "[jit_cold_new_cp] the deferred `new`'s constructor must have run"
    );
    assert_eq!(
        line("clinitAfterCold="),
        "true",
        "[jit_cold_new_cp] the deferred `new` must run the target's <clinit> \
         (JVMS 5.5) before returning the instance"
    );

    // (4) Same for the `anewarray` sibling.
    assert_eq!(
        line("arrClass="),
        "[LColdNewCpProbe$Elem;",
        "[jit_cold_new_cp] the deferred `anewarray` must build an array of the \
         component class the bytecode names"
    );
    assert_eq!(
        line("arrLen="),
        "3",
        "[jit_cold_new_cp] the deferred `anewarray` must honour its length"
    );
    assert_eq!(
        line("arrElem0Null="),
        "true",
        "[jit_cold_new_cp] a fresh reference array must be null-filled"
    );
}
