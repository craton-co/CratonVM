// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for compiled non-tail self-recursion stack recovery.
//!
//! The historical Spring/Groovy failure was an uncatchable native stack fault in
//! compiled recursion. This fixture keeps the shape small and in-tree: warm a
//! non-tail self-recursive static method until the JIT publishes code for it,
//! then call it deeply inside Java `catch (StackOverflowError)`. Passing proves
//! the compiled self-call guard raises a Java-level exception before the native
//! guard page is hit.

#![allow(clippy::unwrap_used)]

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE: &str = "cratonvm/JitDeepRecursionFaultRecovery";

fn classpath_entries() -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(staged) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(staged).exists() {
            entries.push(staged.to_string());
        }
    }
    entries.push(format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR")));
    entries
}

fn fixture_available() -> bool {
    classpath_entries().iter().any(|entry| {
        std::path::Path::new(entry)
            .join("cratonvm/JitDeepRecursionFaultRecovery.class")
            .exists()
    })
}

fn test_vm() -> Vm {
    Vm::new(VmConfig::new().with_classpath(classpath_entries()))
}

/// Pin the warm-up policy for the whole test.
///
/// Both names are *declared* flags, so they are served from the process-wide
/// [`cratonvm_types::flags`] snapshot, not re-read from `environ` — `set_var`
/// only reached the VM here because this binary holds a single test that runs
/// before anything else latches the snapshot. That is exactly the
/// order-dependence that makes this class of test look like a flake the moment
/// a second test is added to the file, so pin it explicitly instead.
///
/// Process-scoped and returned as a guard: `env_cache` memoises both values in
/// their own `OnceLock`s on first read, which happens on whichever thread the
/// VM does its first tier-up check on, so the override has to be visible
/// everywhere and still live at that point.
#[must_use]
fn force_deterministic_jit_warmup() -> cratonvm_types::flags::FlagOverride {
    cratonvm_types::flags::override_process(cratonvm_types::flags::VmFlags::from_env_with_edits(&[
        ("CRATONVM_JIT_THRESHOLD", Some("2")),
        ("CRATONVM_BG_COMPILE", Some("0")),
    ]))
}

#[test]
fn compiled_non_tail_deep_recursion_throws_catchable_stack_overflow() {
    if !fixture_available() {
        eprintln!(
            "Skipping: JitDeepRecursionFaultRecovery.class not available \
             (javac did not stage fixtures and committed class is missing)"
        );
        return;
    }

    let _warmup_policy = force_deterministic_jit_warmup();
    let mut vm = test_vm();

    let ranges_before = cratonvm_jit::jit_code_range_count();
    let warmup = vm.invoke(FIXTURE, "warmupNonTail", "()I", &[]);
    match warmup {
        Ok(Some(Value::Int(128))) => {}
        other => panic!("Expected warmupNonTail() -> Int(128), got: {other:?}"),
    }

    let ranges_after = cratonvm_jit::jit_code_range_count();
    assert!(
        ranges_after > ranges_before,
        "warmupNonTail() must publish at least one JIT code range \
         (before={ranges_before}, after={ranges_after}); otherwise this would \
         only test interpreter StackOverflowError handling"
    );

    let caught = vm.invoke(FIXTURE, "catchOverflowAfterWarmup", "()I", &[]);
    match caught {
        Ok(Some(Value::Int(42))) => {}
        other => panic!(
            "Expected catchOverflowAfterWarmup() to catch compiled recursion \
             StackOverflowError and return 42, got: {other:?}"
        ),
    }
}
