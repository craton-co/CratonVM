// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Advanced exception handling integration tests.
//!
//! These tests exercise complex exception scenarios: nested try/catch,
//! exceptions in catch handlers, finally with return, multi-catch, re-throw,
//! and stack unwinding across method calls.
//!
//! **Prerequisites:**
//! - Java test classes are compiled automatically by `build.rs` if `javac`
//!   is on the PATH. If not, tests will be skipped at runtime.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/ExceptionAdvanced.class")).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

fn printed_ints(vm: &Vm) -> Vec<i32> {
    vm.main_thread
        .printed
        .iter()
        .filter_map(|v| v.as_int())
        .collect()
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: .class files not available (javac not on PATH?)");
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Advanced exception tests (work without RT_JAR via synthetic class hierarchy)
// ---------------------------------------------------------------------------

#[test]
fn test_nested_try_catch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionAdvanced",
        "testNestedTryCatch",
        "()V",
        &[],
    );
    assert!(result.is_ok(), "testNestedTryCatch failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

#[test]
fn test_exception_in_catch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionAdvanced",
        "testExceptionInCatch",
        "()V",
        &[],
    );
    assert!(result.is_ok(), "testExceptionInCatch failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_finally_with_return() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionAdvanced",
        "testFinallyWithReturn",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(42))) => {}
        other => panic!("Expected Ok(Some(Int(42))), got: {other:?}"),
    }
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_multi_catch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ExceptionAdvanced", "testMultiCatch", "()V", &[]);
    assert!(result.is_ok(), "testMultiCatch failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

#[test]
fn test_rethrow() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ExceptionAdvanced", "testRethrow", "()V", &[]);
    assert!(result.is_ok(), "testRethrow failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_stack_unwinding() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionAdvanced",
        "testStackUnwinding",
        "()V",
        &[],
    );
    assert!(result.is_ok(), "testStackUnwinding failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

// ---------------------------------------------------------------------------
// JIT div-by-zero direct-throw regression
//
// The x64 idiv/irem/ldiv/lrem zero-divisor guard used to deopt via
// `jit_uncommon_trap`, which re-ran the WHOLE method from entry — so a side
// effect preceding the trap executed TWICE (HotSpot runs it once). Unlike the
// linear `test_nested_try_catch` above (which runs interpreted), these drive a
// hot helper loop so the divide method tier-ups into JIT'd code, then make a
// single divide-by-zero call. The pre-divide heap-array increment (`c[0]++`)
// must run exactly once, so each delta is 1 (the bug returned 2). See
// `JitDivByZero.java` for why a heap array, not a static field, is the probe.
// ---------------------------------------------------------------------------

fn jit_divzero_delta(method: &str) -> i32 {
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/JitDivByZero", method, "()I", &[]);
    match result {
        Ok(Some(Value::Int(d))) => d,
        other => panic!("{method} returned unexpected value: {other:?}"),
    }
}

#[test]
fn test_jit_idiv_zero_no_double_side_effect() {
    require_class_files!();
    let d = jit_divzero_delta("idivDelta");
    assert_eq!(
        d, 1,
        "idiv side effect ran {d}x (expected 1) — JIT uncommon-trap re-run regression"
    );
}

#[test]
fn test_jit_irem_zero_no_double_side_effect() {
    require_class_files!();
    let d = jit_divzero_delta("iremDelta");
    assert_eq!(
        d, 1,
        "irem side effect ran {d}x (expected 1) — JIT uncommon-trap re-run regression"
    );
}

#[test]
fn test_jit_ldiv_zero_no_double_side_effect() {
    require_class_files!();
    let d = jit_divzero_delta("ldivDelta");
    assert_eq!(
        d, 1,
        "ldiv side effect ran {d}x (expected 1) — JIT uncommon-trap re-run regression"
    );
}

#[test]
fn test_jit_lrem_zero_no_double_side_effect() {
    require_class_files!();
    let d = jit_divzero_delta("lremDelta");
    assert_eq!(
        d, 1,
        "lrem side effect ran {d}x (expected 1) — JIT uncommon-trap re-run regression"
    );
}

#[test]
fn test_jit_divzero_throws_catchable_arithmetic() {
    require_class_files!();
    // verdict: 1=caught w/ "/ by zero", 2=caught w/ other message, 0=not thrown.
    // The JIT-relevant invariant is that the divide-by-zero raises a *catchable*
    // ArithmeticException (1 or 2), not that the VM crashed or swallowed it (0).
    // The exact "/ by zero" message text is validated against HotSpot in the
    // real-JDK CLI differential (scratch/divzero); the synthetic-jdk class
    // hierarchy used here does not carry the HotSpot message string (the
    // interpreter path returns 2 as well — orthogonal to this fix).
    let verdict = jit_divzero_delta("idivMessageOk");
    assert_ne!(
        verdict, 0,
        "JIT div-by-zero did not raise a catchable ArithmeticException (verdict {verdict})"
    );
}
