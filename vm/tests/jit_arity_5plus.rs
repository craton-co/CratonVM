// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT arity-5+ bail-to-interpreter regression (review §2.3, item 4).
//!
//! The JIT call-site dispatcher's register-only ABI table covers
//! 0..=4 args (no-ctx) and 0..=3 args (with-ctx). Methods exceeding
//! that ceiling are routed through `bail_to_interpreter` so the
//! interpreter executes the body. Round-4 wave-2 fixed a previous
//! CRIT where 5+-arg callees silently returned 0; this test pins
//! that the bailout path actually executes the body and returns the
//! correct 64-bit aggregate.
//!
//! Fixture: `vm/tests/resources/cratonvm/JitArity5Plus.java`:
//! `static long sum6(int, int, int, int, int, int)`. 6 int args +
//! long return — comfortably above any of the JIT's register-arg
//! ABI ceilings on both System V AMD64 and win64 conventions. We
//! invoke through 0-arg Java wrappers (`callSmall`, `callLarge`,
//! `drive100`) so the 6-arg dispatch happens via `invokestatic`
//! bytecode (and, once the caller warms up, via
//! `jit_invoke_dispatch` → bailout) rather than via the
//! `Vm::invoke` integration-test entry surface, which has no
//! existing test coverage for multi-arg static call into Java.
//!
//! The three tests:
//!
//! 1. `sum6_small` — cold-invokes `callSmall`. The aggregate
//!    (1+2+3+4+5+6 = 21L) must round-trip whether the JIT
//!    compiled `sum6` or not.
//! 2. `sum6_large` — `callLarge` with inputs 100..600. The
//!    aggregate (2100L) fits in i32, so a regression that lost the
//!    long-widening would still pass on this input — but if the
//!    bailout dropped any one arg the sum would shift by at
//!    least 100, failing immediately.
//! 3. `drive100_aggregate` — `drive100` runs sum6 in a 100-iter
//!    loop so the JIT profiler can register `sum6` as a hot call
//!    site (and potentially admit it through
//!    `try_jit_compile_callee`). The aggregate is independently
//!    computed by the Rust oracle below — every bailout call
//!    contributes to the sum, so an off-by-one in arg passing or
//!    a dropped arg surfaces as a numeric mismatch.

#![allow(clippy::unwrap_used)]

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let path = format!("{}/cratonvm/JitArity5Plus.class", test_resources_dir());
    std::path::Path::new(&path).exists()
}

/// Cargo `cratonvm-vm` test fixtures rely on `javac` having compiled
/// the `.java` sources in `vm/tests/resources/cratonvm/` via the
/// crate `build.rs`. If the compiled class is missing (no `javac` on
/// PATH at build time), skip rather than failing — matches the
/// convention in `interpreter_tests.rs`.
macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: JitArity5Plus.class not available (javac not on PATH?)");
            return;
        }
    };
}

fn test_vm() -> Vm {
    let cfg = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(cfg)
}

// ---------------------------------------------------------------------------
// Cold-path invocations through the 0-arg wrappers.
// ---------------------------------------------------------------------------

#[test]
fn sum6_small_returns_correct_long() {
    require_class_files!();
    let mut vm = test_vm();
    // callSmall() → sum6(1, 2, 3, 4, 5, 6) → 21L
    let r = vm.invoke("cratonvm/JitArity5Plus", "callSmall", "()J", &[]);
    match r {
        Ok(Some(Value::Long(21))) => {}
        other => panic!("Expected Ok(Some(Long(21))), got: {other:?}"),
    }
}

#[test]
fn sum6_large_returns_correct_long_no_arg_dropped() {
    require_class_files!();
    let mut vm = test_vm();
    // callLarge() → sum6(100, 200, 300, 400, 500, 600) → 2100L.
    // A dropped arg would shift the result by >= 100; a narrowing
    // bug at the bailout arg-passing seam would surface here.
    let r = vm.invoke("cratonvm/JitArity5Plus", "callLarge", "()J", &[]);
    match r {
        Ok(Some(Value::Long(2100))) => {}
        other => panic!("Expected Ok(Some(Long(2100))), got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Warm-up driver — gives the JIT profiler a hot call site for
// `sum6` so the bailout path (rather than the pure-interpreter
// path) is the one that actually runs the inner calls.
// ---------------------------------------------------------------------------

/// Independent Rust oracle for `JitArity5Plus.drive100()`. Matches
/// the Java loop body exactly:
///
/// ```text
/// for i in 0..100:
///     total += sum6(i, i+1, i+2, i+3, i+4, i+5)
///           == 6*i + (0+1+2+3+4+5)
///           == 6*i + 15
/// ```
///
/// Closed form: `6 * sum(0..100) + 15*100 = 6*4950 + 1500 = 31_200`.
fn expected_drive100_total() -> i64 {
    let mut total: i64 = 0;
    for i in 0..100i64 {
        total += 6 * i + 15;
    }
    total
}

#[test]
fn drive100_aggregate_matches_oracle() {
    require_class_files!();
    let mut vm = test_vm();
    let expected = expected_drive100_total();
    assert_eq!(expected, 31_200, "oracle drift: closed-form sum changed");

    let r = vm.invoke("cratonvm/JitArity5Plus", "drive100", "()J", &[]);
    match r {
        Ok(Some(Value::Long(actual))) => {
            assert_eq!(
                actual, expected,
                "drive100() aggregate mismatch — \
                 a 6-arg bailout call lost or corrupted an argument",
            );
        }
        other => panic!("Expected Ok(Some(Long({expected}))), got: {other:?}"),
    }
}
