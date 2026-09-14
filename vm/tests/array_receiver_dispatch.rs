// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: an ARRAY receiver must dispatch through `java.lang.Object`
//! (JVMS §4.4.1), never through its component class.
//!
//! A reference array stores its COMPONENT class id in its object header, so
//! `class_id_of(Foo[]) == class_id_of(Foo)` and `class_id_of(long[]) ==
//! ClassId(0)`. Every dispatch path that validates a monomorphic inline-cache
//! entry by comparing that raw class id must therefore FIRST check
//! `kind_of(receiver) == Array` and cede to the slow path.
//!
//! `execute_invokevirtual_vtable_fast` and the JIT's MIC/PIC
//! (`receiver_is_plain_object`, plus the machine-code `OBJECT_KIND_OFFSET`
//! compare in `jit/src/x64.rs`) already did. The interpreter's *inline-cache
//! hit* path in `execute_invokevirtual_cached` did not: warming
//! `invokevirtual java/lang/Object.toString()` on a plain `Foo` and then
//! handing the same call site a `Foo[]` ran `Foo.toString()` with the array as
//! `this`, reading array element 0 as field 0 (`Foo-toString-v0` instead of
//! `[LFoo;@<hash>`). Same for `hashCode`. This is the interpreter half of the
//! already-fixed KC26 `array.clone()` / `ResolvableType[]` family.
//!
//! The probe source is embedded below and compiled to a temp dir on the fly,
//! so the test is self-contained. It skips gracefully when javac, a JDK home
//! or the cratonvm binary are unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class ArrayReceiverDispatchProbe {

    public static class Foo {
        int v;
        Foo(int v) { this.v = v; }
        @Override public String toString() { return "Foo-toString-v" + v; }
        @Override public int hashCode() { return 0x5eed0000 | (v & 0xffff); }
        @Override public boolean equals(Object o) { return true; }
    }

    // One shared call site per Object method: warmed on a plain `Foo`, then
    // handed a `Foo[]`.
    static String callToString(Object o) { return o.toString(); }
    static int callHashCode(Object o) { return o.hashCode(); }
    static boolean callEquals(Object o, Object p) { return o.equals(p); }

    public static void main(String[] args) {
        Foo foo = new Foo(7);
        Foo[] arr = new Foo[3];
        int[] prim = new int[3];

        for (int i = 0; i < 500; i++) {
            callToString(foo);
            callHashCode(foo);
            callEquals(foo, foo);
        }

        String ts = callToString(arr);
        int hc = callHashCode(arr);
        boolean eq = callEquals(arr, foo);

        System.out.println("arrayToString=" + ts);
        System.out.println("arrayHashIsFooOverride=" + ((hc & 0xffff0000) == 0x5eed0000));
        System.out.println("arrayEqualsFoo=" + eq);
        System.out.println("intArrayToString=" + callToString(prim));

        // Reverse order: warm on the array, then hand the site a plain Foo.
        for (int i = 0; i < 500; i++) {
            callToString(arr);
        }
        System.out.println("fooToStringAfterArray=" + callToString(foo));

        // An array's clone() must reach Object's native array copy, not any
        // component-class body (the shape behind the H2 TestTempTables
        // `Thread.clone` / CloneNotSupportedException report).
        long[] bits = new long[4];
        bits[2] = 42L;
        long[] copy = java.util.Arrays.copyOf(bits, bits.length);
        System.out.println("longCloneOk=" + (copy != bits && copy.length == 4 && copy[2] == 42L));
        Foo[] arrCopy = arr.clone();
        System.out.println("refCloneOk=" + (arrCopy != arr && arrCopy.length == 3));

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
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-array-receiver-dispatch-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("ArrayReceiverDispatchProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("ArrayReceiverDispatchProbe.class"));
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
            eprintln!("[array_receiver_dispatch] javac could not be executed: {e}; skipping");
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
                "[array_receiver_dispatch] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("ArrayReceiverDispatchProbe.class").exists(),
        "[array_receiver_dispatch] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, nojit: bool) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home").arg(jdk);
    if nojit {
        cmd.arg("--nojit");
    }
    cmd.arg("-c")
        .arg(classes)
        .arg("ArrayReceiverDispatchProbe")
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
                    panic!("[array_receiver_dispatch] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[array_receiver_dispatch] try_wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect output");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn assert_array_receiver_dispatch(stdout: &str, stderr: &str, mode: &str) {
    let line = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| {
                panic!(
                    "[array_receiver_dispatch/{mode}] missing `{key}` line.\n\
                     stdout:\n{stdout}\nstderr:\n{stderr}"
                )
            })
            .to_string()
    };

    let ts = line("arrayToString=");
    assert!(
        ts.starts_with("[LArrayReceiverDispatchProbe$Foo;@"),
        "[{mode}] Foo[] must use Object.toString (`[LFoo;@hash`), \
         not the component class's override — got `{ts}`"
    );
    assert_eq!(
        line("arrayHashIsFooOverride="),
        "false",
        "[{mode}] Foo[].hashCode() must be Object's identity hash, \
         not Foo.hashCode()"
    );
    assert_eq!(
        line("arrayEqualsFoo="),
        "false",
        "[{mode}] Foo[].equals(foo) must be Object's identity comparison"
    );
    let its = line("intArrayToString=");
    assert!(
        its.starts_with("[I@"),
        "[{mode}] int[].toString() must be `[I@hash` — got `{its}`"
    );
    assert_eq!(
        line("fooToStringAfterArray="),
        "Foo-toString-v7",
        "[{mode}] a plain Foo must still reach Foo.toString() after the \
         same call site saw an array receiver"
    );
    assert_eq!(
        line("longCloneOk="),
        "true",
        "[{mode}] long[].clone() (via Arrays.copyOf) must produce an \
         independent copy"
    );
    assert_eq!(
        line("refCloneOk="),
        "true",
        "[{mode}] Foo[].clone() must produce an independent copy"
    );
    assert!(
        stdout.contains("OK"),
        "[{mode}] probe did not finish.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn array_receiver_dispatches_through_object_not_component_class() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[array_receiver_dispatch] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`. Skipping."
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[array_receiver_dispatch] no JDK home \
                 (set CRATONVM_TEST_JDK or JAVA_HOME); skipping"
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
            eprintln!("[array_receiver_dispatch] javac unavailable; skipping");
            return;
        }
    };

    for (mode, nojit) in [("nojit", true), ("jit", false)] {
        let (stdout, stderr) = run_probe(&bin, &jdk, &classes, nojit);
        assert_array_receiver_dispatch(&stdout, &stderr, mode);
    }
}
