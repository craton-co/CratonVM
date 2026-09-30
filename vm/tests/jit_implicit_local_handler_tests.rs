// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An IMPLICIT exception raised by compiled code — a bounds check, an inline
//! null check, a zero-divisor guard — must be able to enter this method's own
//! compiled `catch` block, and must give the byte-identical answers it gave
//! when it left the frame instead.
//!
//! # What changed, and why it is a separate arm
//!
//! The compiled-local-handler feature (2026-08-20,
//! `jit/src/x64/deopt_stubs.rs::emit_local_handler_stubs`) covered the fallible
//! sites that hand back a *throwable*: a dispatched callee's exception and an
//! allocation's `OutOfMemoryError`. `jit_local_handler_lookup` deliberately
//! refused the three implicit traps, because each of them signals a REQUEST
//! for a throwable — a flag plus an index/length or a JEP-358 action code —
//! and nothing in compiled code could grant one.
//!
//! So the whole implicit family kept the round trip
//! `docs/internal/fixed-bugs/perf-implicit-exceptions-in-compiled-code-cost-microseconds-each-FIXED-20260920.md`
//! measures: an exception-exit TRANSFER, an interpreted handler, and — inside
//! an OSR'd loop — a fresh OSR entry at the next back edge, about 1.4 us of a
//! 3.0-3.3 us total against HotSpot's 0.4-0.7 for the entire thing.
//!
//! `local_handler_enter_implicit` (`vm/src/jit/helpers.rs`) grants the request
//! through `materialize_implicit_signal`, the same body the interpreter drain
//! and the callee door already use, so the throwable's class, message and
//! trace cannot depend on which door built it.
//!
//! # What this test is actually asserting
//!
//! Two runs of one probe: default (the feature ON) and
//! `CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS=0` (the pre-2026-09-20 route, byte
//! for byte). Every printed line must match, AND the ON run must report a
//! non-zero `implicit-entered` in the `[cratonvm] local handlers:` census.
//! Without that second half a green proves nothing: a feature that never
//! engaged agrees with its own control on every line.
//!
//! The probe covers the shapes a naive implementation gets wrong:
//!
//!  * two sequential protected ranges catching the SAME type — the handler
//!    must be the one whose range covers the throwing bci, which is the defect
//!    `find_jit_exception_handler`'s pc-unknown search has;
//!  * a typed handler that does NOT match — the throw must escape the method
//!    with every signal restored, not be swallowed;
//!  * a catch-all (`finally`) — it matches anything, including an implicit
//!    trap;
//!  * a nested `try` whose inner handler catches a different type;
//!  * the throwable is a real object: its message and its stack trace must
//!    survive being built inside the frame that raised it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

/// Every kernel throws on EVERY iteration and is called far past the JIT
/// threshold, so the `catch` runs in compiled code for the bulk of the run.
///
/// `A` has length 4 and every index used against it is `>= 8`, so no
/// speculative bounds-check elimination can survive and the pad is reached
/// each time.
const PROBE_SRC: &str = r#"
public class ImplicitLocalHandlerProbe {
    static final int[] A = {10, 11, 12, 13};

    static int at(int[] a, int i) { return a[i]; }
    static int div(int x, int y) { return x / y; }
    static int len(int[] a) { return a.length; }

    // Two protected ranges, one catch type. A handler picked by type alone
    // runs the FIRST row whichever range threw.
    static int twoRanges(int i) {
        int s = 0;
        try { s += A[i]; } catch (ArrayIndexOutOfBoundsException e) { s += 1000; }
        try { s += A[i + 1]; } catch (ArrayIndexOutOfBoundsException e) { s += 2000; }
        return s;
    }

    // The handler does not take an AIOOBE: the throw must leave this method.
    static int wrongType(int i) {
        try { return A[i]; } catch (NullPointerException e) { return -1; }
    }

    // A catch-all takes an implicit trap too.
    static int finallyCounts(int i) {
        int n = 0;
        try { n += A[i]; } finally { n += 1; }
        return n;
    }

    // The inner handler is for a type that cannot match; the outer one takes it.
    static String nested(int i) {
        try {
            try { return "v" + A[i]; }
            catch (ArithmeticException e) { return "inner-ae"; }
        } catch (ArrayIndexOutOfBoundsException e) {
            return "outer:" + e.getMessage();
        }
    }

    static int npeCaught(int[] a) {
        try { return len(a); } catch (NullPointerException e) { return -5; }
    }

    static String npeMessage(int[] a) {
        try { return "len" + len(a); }
        catch (NullPointerException e) { return e.getMessage() == null ? "null" : "msg"; }
    }

    static int divCaught(int i) {
        try { return div(100, i); } catch (ArithmeticException e) { return -7; }
    }

    static int localDivCaught(int i) {
        try { return 100 / i; } catch (ArithmeticException e) { return -9; }
    }

    // A throwable entered locally must still be a real object with a trace.
    static int traceDepth(int i) {
        try { return at(A, i); }
        catch (ArrayIndexOutOfBoundsException e) { return e.getStackTrace().length > 0 ? 1 : 0; }
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 60000;
        long a = 0, c = 0, d = 0, e = 0, g = 0;
        int escaped = 0, finallyEscaped = 0, traced = 0, npeMsgNull = 0;
        String lastNested = "";
        for (int k = 0; k < iters; k++) {
            int i = 3 + (k & 1);                 // 3 in range, 4 out of range
            a += twoRanges(i);
            try { wrongType(i + 5); } catch (ArrayIndexOutOfBoundsException x) { escaped++; }
            try { c += finallyCounts(i + 5); } catch (ArrayIndexOutOfBoundsException x) { finallyEscaped++; }
            g += finallyCounts(0);
            lastNested = nested(i + 5);
            d += npeCaught((k & 1) == 0 ? null : A);
            if ("null".equals(npeMessage((k & 1) == 0 ? null : A))) { npeMsgNull++; }
            e += divCaught(k % 3 == 0 ? 0 : 2);
            e += localDivCaught(k % 4 == 0 ? 0 : 5);
            traced += traceDepth(i + 5);
        }
        System.out.println("a=" + a);
        System.out.println("c=" + c);
        System.out.println("d=" + d);
        System.out.println("e=" + e);
        System.out.println("g=" + g);
        System.out.println("escaped=" + escaped);
        System.out.println("finallyEscaped=" + finallyEscaped);
        System.out.println("traced=" + traced);
        System.out.println("npeMsgNull=" + npeMsgNull);
        System.out.println("nested=" + lastNested);
        System.out.println("OK");
    }
}
"#;

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
    let dir = std::env::temp_dir().join("cratonvm-jit-implicit-local-handler-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("ImplicitLocalHandlerProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("ImplicitLocalHandlerProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "17", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_implicit_local_handler] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    assert!(
        out.status.success() && dir.join("ImplicitLocalHandlerProbe.class").exists(),
        "[jit_implicit_local_handler] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, feature_on: bool) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        // The engagement census: `[cratonvm] local handlers: ... implicit-entered=N`.
        .env("CRATONVM_DBG_JIT_METHOD_STATS", "1")
        .arg("-c")
        .arg(classes)
        .arg("ImplicitLocalHandlerProbe")
        .arg("60000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !feature_on {
        cmd.env("CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS", "0");
    }
    let child = cmd.spawn().expect("spawn cratonvm");

    // `common::wait_draining`, not a `try_wait` poll loop, and the difference
    // is a ten-minute deadlock.
    //
    // This function used to poll `try_wait` and read neither pipe until the
    // child had exited. A piped stream nobody reads blocks its WRITER once the
    // OS buffer fills, so the probe stalled inside its own exit path and the
    // poll loop watched it not finish for the whole cap. Measured on Windows,
    // 2026-09-22: `CRATONVM_DBG_JIT_METHOD_STATS=1` — which this function sets,
    // because the engagement census is half of what the test asserts — makes
    // the probe write **136 KB** of stderr at shutdown. The same invocation
    // with its pipes drained finishes in about two minutes; under the poll loop
    // it reported `probe timed out after 600s`, deterministically, on an idle
    // host, in a way that reads like the VM hanging.
    //
    // `wait_draining`'s own doc comment describes this exact shape and says it
    // is "everywhere in `vm/tests`" — it was written for
    // `class_loader_unload_regression`, whose stderr was 101,808 bytes, and it
    // cost that investigation a session. This file is one more instance of it,
    // and using the helper rather than re-deriving the fix is the point of
    // having one.
    let timeout = Duration::from_secs(600);
    let finished = common::wait_draining(child, timeout);
    assert!(
        !finished.timed_out,
        "[jit_implicit_local_handler] probe timed out after {timeout:?}. Captured so far:\n\
         --- stdout ---\n{}\n--- stderr (tail) ---\n{}",
        String::from_utf8_lossy(&finished.output.stdout),
        String::from_utf8_lossy(&finished.output.stderr)
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>(),
    );
    (
        String::from_utf8_lossy(&finished.output.stdout).into_owned(),
        String::from_utf8_lossy(&finished.output.stderr).into_owned(),
    )
}

/// The probe's own `key=value` lines, in order, ignoring anything the VM
/// prints around them.
fn answers(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.contains('=')
                && !l.starts_with('[')
                && l.split('=')
                    .next()
                    .is_some_and(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric()))
        })
        .map(str::to_string)
        .collect()
}

/// `implicit-entered` from the `[cratonvm] local handlers:` census line, or
/// `None` when the line is absent.
fn implicit_entered(stderr: &str) -> Option<u64> {
    stderr
        .lines()
        .find(|l| l.contains("[cratonvm] local handlers:"))
        .and_then(|l| {
            l.split_whitespace()
                .find_map(|w| w.strip_prefix("implicit-entered="))
                .and_then(|v| v.parse().ok())
        })
}

#[test]
fn an_implicit_exception_enters_its_own_compiled_handler_with_the_same_answers() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_implicit_local_handler] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_implicit_local_handler] no usable JDK found (set CRATONVM_TEST_JDK or \
             JAVA_HOME). skipping."
        );
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

    let (on_out, on_err) = run_probe(&bin, &jdk, &classes, true);
    let (off_out, off_err) = run_probe(&bin, &jdk, &classes, false);

    for (label, out, err) in [("on", &on_out, &on_err), ("off", &off_out, &off_err)] {
        assert!(
            out.contains("OK"),
            "[jit_implicit_local_handler] the {label} arm did not reach its final marker.\n\
             stdout:\n{out}\nstderr:\n{err}"
        );
    }

    // ANTI-VACUITY FIRST. A feature that never engaged agrees with its own
    // control on every line, so the comparison below is worth nothing until
    // this holds.
    let entered = implicit_entered(&on_err);
    assert!(
        entered.is_some_and(|n| n > 0),
        "[jit_implicit_local_handler] the ON arm reported implicit-entered={entered:?} — no \
         implicit exception entered a compiled handler, so this run says nothing about the \
         feature. Census line:\n{}",
        on_err
            .lines()
            .find(|l| l.contains("local handlers:"))
            .unwrap_or("<census line absent>")
    );
    assert_eq!(
        implicit_entered(&off_err),
        Some(0),
        "[jit_implicit_local_handler] the kill switch did not close the door: the OFF arm is \
         supposed to be the pre-2026-09-20 route, in which no implicit trap can enter a \
         compiled handler."
    );

    assert_eq!(
        answers(&on_out),
        answers(&off_out),
        "[jit_implicit_local_handler] entering an implicit exception's handler in compiled \
         code changed an observable answer. The two runs differ only in \
         CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS.\n\
         ON stdout:\n{on_out}\nOFF stdout:\n{off_out}"
    );

    // And the shapes the comparison alone cannot pin down: an answer that is
    // wrong in BOTH arms would still compare equal.
    let on = answers(&on_out);
    let get = |k: &str| {
        on.iter()
            .find_map(|l| l.strip_prefix(k))
            .unwrap_or_else(|| panic!("probe must print {k}; got {on:?}"))
            .to_string()
    };
    assert_eq!(
        get("escaped="),
        "60000",
        "a `catch (NullPointerException)` must not take an AIOOBE — every one of these has to \
         escape `wrongType` and be caught by the caller"
    );
    assert_eq!(
        get("finallyEscaped="),
        "60000",
        "a `finally` runs and then re-raises: the AIOOBE still leaves `finallyCounts`"
    );
    assert_eq!(
        get("g="),
        "660000",
        "`finallyCounts(0)` is in range: A[0] + 1 == 11, 60 000 times, and the `finally` must \
         not have run twice on a non-exceptional path"
    );
    assert_eq!(
        get("traced="),
        "60000",
        "a throwable entered locally is a real object and keeps its stack trace — the default \
         configuration builds every implicit exception in full \
         (`CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` is OFF)"
    );
    assert!(
        get("nested=").starts_with("outer:"),
        "the inner `catch (ArithmeticException)` cannot take an AIOOBE; the outer one must"
    );
}
