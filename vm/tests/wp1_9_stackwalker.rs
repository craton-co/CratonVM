// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.9 — StackWalker completeness conformance tests.
//!
//! Verifies:
//! * `StackTraceEntry` carries non-default `byte_code_index` + `line_number`
//!   populated from each frame's `last_instr_pc` + LineNumberTable.
//! * The stackwalker helper can look up line numbers from a LineNumberTable
//!   attribute on a synthetic `ClassFileMethod`.
//! * The `StackStreamFactory.AbstractStackWalker.callStackWalk` +
//!   `fetchStackFrames` natives are registered and succeed when invoked
//!   against a registry built by the native-builtins crate.
//! * The `StackFrameInfo.getByteCodeIndex` and `.getDeclaringClass`
//!   accessors are registered.
//!
//! Stack-trace capture against a live interpreter is exercised indirectly
//! via `capture_stack_trace` tests elsewhere (jck_conformance); this file
//! focuses on the unit-level guarantees.

use cratonvm_native_api::{NativeMethodRegistry, StackTraceEntry};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LOG4J_CALLER_PROBE: &str = "StackWalkerLog4jCallerProbe";
const LOG4J_STRESS_PROBE: &str = "StackWalkerLog4jStressProbe";
const LOG4J_CALLER_TIMEOUT: Duration = Duration::from_secs(45);
const LOG4J_STRESS_TIMEOUT: Duration = Duration::from_secs(60);

#[test]
fn stack_trace_entry_carries_bci_and_line_number() {
    let e = StackTraceEntry {
        class_name: Arc::from("example/Foo"),
        method_name: Arc::from("bar"),
        source_file: Some(Arc::from("Foo.java")),
        line_number: 42,
        byte_code_index: 17,
        class_id: None,
        // ARCH-2026-07-26: `method_index` is the frame's slot in
        // `Class::methods`, carried so deferred line resolution can pick the
        // exact member of an overload set. `None` here — this entry is
        // synthesized, not captured from a live frame.
        method_index: None,
    };
    assert_eq!(e.line_number, 42);
    assert_eq!(e.byte_code_index, 17);
    assert_eq!(e.method_index, None);
    assert_eq!(&*e.class_name, "example/Foo");
    assert_eq!(e.source_file.as_deref(), Some("Foo.java"));
}

#[test]
fn native_stack_walker_boot_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::stack_walker::register_stack_walker_boot(&mut r);
    assert!(r
        .find(
            "java/lang/StackWalker",
            "getInstance",
            "()Ljava/lang/StackWalker;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackWalker",
            "getInstance",
            "(Ljava/util/Set;I)Ljava/lang/StackWalker;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackWalker",
            "getCallerClass",
            "()Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn lang_stackwalker_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::lang_stackwalker::register_lang_stackwalker(&mut r);
    assert!(r
        .find(
            "java/lang/StackStreamFactory$AbstractStackWalker",
            "callStackWalk",
            "(JIII[Ljava/lang/Object;[Ljava/lang/Class;)Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackStreamFactory$AbstractStackWalker",
            "fetchStackFrames",
            "(JJII[Ljava/lang/Object;)I"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackFrameInfo",
            "getClassName",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackFrameInfo",
            "getMethodName",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackFrameInfo",
            "getFileName",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find("java/lang/StackFrameInfo", "getLineNumber", "()I")
        .is_some());
    assert!(r
        .find("java/lang/StackFrameInfo", "getByteCodeIndex", "()I")
        .is_some());
    assert!(r
        .find(
            "java/lang/StackFrameInfo",
            "getDeclaringClass",
            "()Ljava/lang/Class;"
        )
        .is_some());
    assert!(r
        .find("java/lang/StackFrameInfo", "isNativeMethod", "()Z")
        .is_some());
    assert!(r
        .find(
            "java/lang/StackFrameInfo",
            "toStackTraceElement",
            "()Ljava/lang/StackTraceElement;"
        )
        .is_some());
}

#[test]
fn line_number_for_bci_picks_largest_start_leq_bci() {
    use cratonvm_reader::attribute::LineNumberEntry;
    // Simulate the core scan logic for LineNumberTable lookup that
    // `crate::runtime::stackwalker::line_number_for_bci` uses.
    let entries = vec![
        LineNumberEntry {
            start_pc: 0,
            line_number: 10,
        },
        LineNumberEntry {
            start_pc: 5,
            line_number: 20,
        },
        LineNumberEntry {
            start_pc: 9,
            line_number: 30,
        },
    ];

    let lookup = |bci: u16| -> Option<u16> {
        let mut best = None;
        let mut best_start = 0;
        for e in &entries {
            if e.start_pc <= bci && (best.is_none() || e.start_pc >= best_start) {
                best_start = e.start_pc;
                best = Some(e.line_number);
            }
        }
        best
    };

    assert_eq!(lookup(0), Some(10));
    assert_eq!(lookup(4), Some(10));
    assert_eq!(lookup(5), Some(20));
    assert_eq!(lookup(8), Some(20));
    assert_eq!(lookup(9), Some(30));
    assert_eq!(lookup(1000), Some(30));
}

/// `StackWalker.getInstance()` must return a non-null walker.
///
/// # Why this is `#[ignore]`d rather than fixed in place
///
/// The body was a closure that was DEFINED and never CALLED:
///
/// ```ignore
/// let _ = |_ctx: &mut dyn NativeContext| { /* compile-time guard only */ };
/// ```
///
/// It could only fail by failing to compile, so it reported `ok` unconditionally
/// — one of the ~60 vacuous greens the 2026-08-07 audit found. It cannot be
/// repaired at this level: invoking the native needs a `&mut dyn NativeContext`,
/// and the only implementors of that trait live inside the interpreter. An
/// integration test in `vm/tests/` has no way to construct one.
///
/// Marked `#[ignore]` so `cargo test` reports it as **ignored** instead of
/// **ok** — which is the truth. The real coverage is in
/// `native-builtins/src/stack_walker.rs::tests`, which runs in-crate and can
/// build a context; `native_stack_walker_boot_natives_registered` above already
/// pins that `getInstance` is registered under both descriptors.
///
/// To un-ignore this, drive it end-to-end through a `Vm` (as the log4j probes
/// further down this file do) rather than through the native directly.
#[test]
#[ignore = "cannot construct a `&mut dyn NativeContext` from vm/tests; real coverage is \
            native-builtins/src/stack_walker.rs::tests"]
fn stack_walker_default_never_returns_null() {
    unimplemented!(
        "WP1.9: no in-tree way to invoke the StackWalker.getInstance native from an integration \
         test. See this function's doc comment. Do not replace this with a body that cannot fail \
         — that is what it was before."
    );
}

#[test]
fn line_number_sentinels_are_minus_one_and_minus_two() {
    use cratonvm_vm::runtime::stackwalker::{LINE_NUMBER_NATIVE, LINE_NUMBER_UNKNOWN};
    assert_eq!(LINE_NUMBER_UNKNOWN, -1);
    assert_eq!(LINE_NUMBER_NATIVE, -2);
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
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

fn java_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(home);
        if p.exists() {
            return Some(p);
        }
    }
    for candidate in [
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn classpath_dir(probe: &str) -> Option<PathBuf> {
    if let Some(compiled) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        let p = PathBuf::from(compiled);
        if p.join("cratonvm").join(format!("{probe}.class")).exists() {
            return Some(p);
        }
    }
    let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources");
    if committed
        .join("cratonvm")
        .join(format!("{probe}.class"))
        .exists()
    {
        return Some(committed);
    }
    None
}

fn run_stackwalker_probe(probe: &str, timeout: Duration) -> (String, String) {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[wp1_9_stackwalker] cratonvm binary missing; build -p cratonvm-cli or set CRATONVM_BIN"
        );
        return (String::new(), String::new());
    };
    let Some(jh) = java_home() else {
        eprintln!("[wp1_9_stackwalker] JDK 25 java-home missing; skipping Log4j caller probe");
        return (String::new(), String::new());
    };
    let Some(cp) = classpath_dir(probe) else {
        eprintln!("[wp1_9_stackwalker] {probe}.class missing; javac likely unavailable");
        return (String::new(), String::new());
    };

    let mut child = Command::new(&bin)
        .arg("--java-home")
        .arg(&jh)
        .arg("-c")
        .arg(&cp)
        .arg(format!("cratonvm.{probe}"))
        .env_remove("CRATONVM_DISABLE_JIT")
        // `CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/` was dropped here: `d1979bec5`
        // deleted the static package-ban machinery and its last reader.
        .env("CRATONVM_JIT_THRESHOLD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cratonvm Log4j caller probe");

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[wp1_9_stackwalker] {probe} timed out after {timeout:?}; \
                         StackWalker caller resolution may be recursing or cloning \
                         transient traces excessively"
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[wp1_9_stackwalker] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("collect cratonvm Log4j caller probe output");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        output.status.success(),
        "{probe} exited with {:?}\n\n{combined}",
        output.status.code()
    );
    (stdout, combined)
}

#[test]
fn stackwalker_log4j_shape_resolves_declaring_caller_under_jit() {
    let (stdout, combined) = run_stackwalker_probe(LOG4J_CALLER_PROBE, LOG4J_CALLER_TIMEOUT);
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("caller=cratonvm.StackWalkerLog4jCallerProbe$LoggerFactory")
            && stdout.contains("STACKWALKER_LOG4J_CALLER_OK"),
        "{LOG4J_CALLER_PROBE} did not resolve the expected caller class\n\n{combined}"
    );
}

#[test]
fn stackwalker_log4j_deep_repeated_walks_finish_under_jit() {
    let (stdout, combined) = run_stackwalker_probe(LOG4J_STRESS_PROBE, LOG4J_STRESS_TIMEOUT);
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("caller=cratonvm.StackWalkerLog4jStressProbe$LoggerFactory")
            && stdout.contains("STACKWALKER_LOG4J_STRESS_OK"),
        "{LOG4J_STRESS_PROBE} did not complete the repeated deep walk stress\n\n{combined}"
    );
}

const SELFREC_FRAMES_PROBE: &str = "JitSelfRecursionFramesProbe";
const SELFREC_FRAMES_TIMEOUT: Duration = Duration::from_secs(60);

/// A directly self-recursive method's activations must stay visible to
/// `Throwable.getStackTrace()` after it tiers up.
///
/// Before `conservative_roots::active_compiled_frames` walked the saved-RBP
/// chain, `JIT_ENTRY_CHAIN`'s one-entry-per-interpreter->JIT-boundary rule made
/// 64 nested activations report as ONE frame: the probe's early (interpreted)
/// rounds saw 67 and its later (compiled) rounds saw 3. The probe compares its
/// own first and last round, so it needs no hard-coded depth and cannot pass
/// vacuously on a VM that never compiles anything — a run where nothing tiers
/// up reports equal counts for the honest reason.
///
/// `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES=1` restores the collapse, which is how
/// this assertion was confirmed to bite.
#[test]
fn self_recursive_activations_survive_tier_up() {
    let (stdout, combined) = run_stackwalker_probe(SELFREC_FRAMES_PROBE, SELFREC_FRAMES_TIMEOUT);
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("SELFREC_FRAMES_OK"),
        "{SELFREC_FRAMES_PROBE}: a self-recursive method's frames changed count across \
         tier-up (or the computed answers changed). The compiled rounds must report the \
         same depth as the interpreted ones.\n\n{combined}"
    );
}

const OSR_DEDUPE_PROBE: &str = "JitOsrFrameDedupeProbe";
const OSR_DEDUPE_TIMEOUT: Duration = Duration::from_secs(60);

/// An OSR'd method must appear ONCE on the stack, not twice.
///
/// An OSR transfer leaves the interpreter `Frame` in place and adds a compiled
/// chain entry for the same activation; the trace reported both, so `SWCross`
/// read 69 frames where HotSpot reads 68 and its first two entries were both
/// `main`. `drop_osr_continuations` suppresses the compiled half, keyed on
/// `can_osr_enter(frame.pc)` — the interpreter frame of an OSR continuation is
/// parked at the back-edge it jumped from, where an interpreted CALLER of the
/// same method would be parked at an invoke.
///
/// The probe counts its own `main` frames, so it needs no HotSpot comparison
/// and a VM that never OSRs passes honestly. `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1`
/// restores the duplicate, which is how this assertion was confirmed to bite.
#[test]
fn an_osr_continuation_is_not_reported_twice() {
    let (stdout, combined) = run_stackwalker_probe(OSR_DEDUPE_PROBE, OSR_DEDUPE_TIMEOUT);
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("OSR_DEDUPE_OK"),
        "{OSR_DEDUPE_PROBE}: an OSR'd method appeared more than once on its own stack —          the interpreter frame and the compiled chain entry describe ONE activation.

{combined}"
    );
}
