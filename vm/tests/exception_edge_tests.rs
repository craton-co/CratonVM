// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Exception handling edge-case tests (Session 2 hardening).
//!
//! Tests cover: finally semantics, deep unwinding, catch priority,
//! ExceptionInInitializerError wrapping, return-from-try/catch,
//! exception-in-finally, cross-interface exceptions, and more.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::{MethodCallFailed, MethodCallResult};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/ExceptionEdgeCases.class")).exists()
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

fn describe_result(vm: &Vm, result: &MethodCallResult) -> String {
    match result {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let class_id = vm.shared.mem.heap.class_id_of(*exc);
            let class_name = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|class| class.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {:?}>", class_id));
            format!("Err(ExceptionThrown({class_name}, {exc:?}))")
        }
        other => format!("{other:?}"),
    }
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: ExceptionEdgeCases.class not available");
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Edge-case tests
// ---------------------------------------------------------------------------

#[test]
fn test_finally_on_normal_return() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testFinallyOnNormalReturn",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testFinallyOnNormalReturn failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

#[test]
fn test_finally_on_exception() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testFinallyOnException",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testFinallyOnException failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

#[test]
fn test_exception_in_finally() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testExceptionInFinally",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testExceptionInFinally failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_deep_unwinding() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testDeepUnwinding",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testDeepUnwinding failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_catch_superclass() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testCatchSuperclass",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testCatchSuperclass failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1]);
}

#[test]
fn test_first_matching_catch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testFirstMatchingCatch",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testFirstMatchingCatch failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1]);
}

#[test]
fn test_return_from_try_with_finally() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testReturnFromTryWithFinally",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(42))) => {}
        other => panic!(
            "Expected Ok(Some(Int(42))), got: {}",
            describe_result(&vm, &other)
        ),
    }
    assert_eq!(printed_ints(&vm), vec![1]);
}

#[test]
fn test_return_from_catch_with_finally() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testReturnFromCatchWithFinally",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(99))) => {}
        other => panic!(
            "Expected Ok(Some(Int(99))), got: {}",
            describe_result(&vm, &other)
        ),
    }
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_catch_all_after_specific() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testCatchAllAfterSpecific",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testCatchAllAfterSpecific failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_null_check_in_catch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testNullCheckInCatch",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testNullCheckInCatch failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_rethrow_preserves_identity() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testRethrowPreservesIdentity",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testRethrowPreservesIdentity failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_clinit_exception() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testClinitException",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testClinitException failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1]);
}

#[test]
fn test_finally_in_loop() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testFinallyInLoop",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testFinallyInLoop failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

#[test]
fn test_chained_exceptions() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ExceptionEdgeCases",
        "testChainedExceptions",
        "()V",
        &[],
    );
    assert!(
        result.is_ok(),
        "testChainedExceptions failed: {}",
        describe_result(&vm, &result)
    );
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// Unit tests for refs_equal (if_acmpeq/if_acmpne correctness)
// ---------------------------------------------------------------------------

#[test]
fn test_refs_equal_same_object() {
    use cratonvm_vm::runtime::interpreter::test_refs_equal;
    let vm = test_vm();
    let obj = vm
        .shared
        .mem
        .heap
        .alloc_object(cratonvm_vm::classloading::ClassId::new(0), 0);
    let a = Value::Object(Some(obj));
    let b = Value::Object(Some(obj));
    assert!(test_refs_equal(&a, &b), "same object should be equal");
}

#[test]
fn test_refs_equal_different_objects() {
    use cratonvm_vm::runtime::interpreter::test_refs_equal;
    let vm = test_vm();
    let obj1 = vm
        .shared
        .mem
        .heap
        .alloc_object(cratonvm_vm::classloading::ClassId::new(0), 0);
    let obj2 = vm
        .shared
        .mem
        .heap
        .alloc_object(cratonvm_vm::classloading::ClassId::new(0), 0);
    let a = Value::Object(Some(obj1));
    let b = Value::Object(Some(obj2));
    assert!(
        !test_refs_equal(&a, &b),
        "different objects should not be equal"
    );
}

#[test]
fn test_refs_equal_both_null() {
    use cratonvm_vm::runtime::interpreter::test_refs_equal;
    let a = Value::Object(None);
    let b = Value::Object(None);
    assert!(test_refs_equal(&a, &b), "both null should be equal");
}

/// `if_acmpeq` on two JNI-smuggled null handles.
///
/// A `jobject` null crossing the operand stack as raw bits arrives as
/// `Value::Long(0)`. `ref_operand_is_null` calls that the null reference, and
/// `refs_equal` already answered `true` for the MIXED pair
/// (`Long(0)` vs `Object(None)`) — but the pair where BOTH sides are the
/// smuggled form had no arm, so `if_acmpeq(nullHandle, nullHandle)` answered
/// `false` while `if_acmpeq(nullHandle, null)` answered `true` and
/// `ifnull(nullHandle)` answered `true`. Reference equality has to be
/// reflexive on the value that all three agree is null.
///
/// Delete the `(Value::Long(0), Value::Long(0))` arm of `refs_equal` and the
/// first assertion below fails.
#[test]
fn test_refs_equal_jni_null_handle_is_reflexive() {
    use cratonvm_vm::runtime::interpreter::test_refs_equal;
    let handle = Value::Long(0);
    assert!(
        test_refs_equal(&handle, &handle),
        "a JNI null handle must equal itself"
    );
    assert!(
        test_refs_equal(&Value::Long(0), &Value::Object(None)),
        "a JNI null handle must equal a plain null (pre-existing contract)"
    );
    assert!(
        test_refs_equal(&Value::Object(None), &Value::Long(0)),
        "and symmetrically"
    );
    // A non-zero long is a live jobject pointer or an honest long -- never the
    // null reference, so two different ones must not collapse to equal.
    assert!(
        !test_refs_equal(&Value::Long(0), &Value::Long(8)),
        "null handle vs non-null handle"
    );
}

#[test]
fn test_refs_equal_null_vs_nonnull() {
    use cratonvm_vm::runtime::interpreter::test_refs_equal;
    let vm = test_vm();
    let obj = vm
        .shared
        .mem
        .heap
        .alloc_object(cratonvm_vm::classloading::ClassId::new(0), 0);
    let a = Value::Object(None);
    let b = Value::Object(Some(obj));
    assert!(
        !test_refs_equal(&a, &b),
        "null vs non-null should not be equal"
    );
    assert!(
        !test_refs_equal(&b, &a),
        "non-null vs null should not be equal"
    );
}

#[test]
fn test_refs_equal_int_zero_vs_null() {
    use cratonvm_vm::runtime::interpreter::test_refs_equal;
    let a = Value::Int(0);
    let b = Value::Object(None);
    assert!(
        test_refs_equal(&a, &b),
        "Int(0) should equal null in autoboxed context"
    );
}
