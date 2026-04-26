//! Advanced exception handling integration tests.
//!
//! These tests exercise complex exception scenarios: nested try/catch,
//! exceptions in catch handlers, finally with return, multi-catch, re-throw,
//! and stack unwinding across method calls.
//!
//! **Prerequisites:**
//! - Java test classes are compiled automatically by `build.rs` if `javac`
//!   is on the PATH. If not, tests will be skipped at runtime.

use rustjvm_vm::config::VmConfig;
use rustjvm_vm::types::Value;
use rustjvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/rustjvm/ExceptionAdvanced.class")).exists()
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
        "rustjvm/ExceptionAdvanced",
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
        "rustjvm/ExceptionAdvanced",
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
        "rustjvm/ExceptionAdvanced",
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
    let result = vm.invoke("rustjvm/ExceptionAdvanced", "testMultiCatch", "()V", &[]);
    assert!(result.is_ok(), "testMultiCatch failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2, 3]);
}

#[test]
fn test_rethrow() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("rustjvm/ExceptionAdvanced", "testRethrow", "()V", &[]);
    assert!(result.is_ok(), "testRethrow failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}

#[test]
fn test_stack_unwinding() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "rustjvm/ExceptionAdvanced",
        "testStackUnwinding",
        "()V",
        &[],
    );
    assert!(result.is_ok(), "testStackUnwinding failed: {result:?}");
    assert_eq!(printed_ints(&vm), vec![1, 2]);
}
