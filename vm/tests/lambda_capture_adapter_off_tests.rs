// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The other half of the capturing-thunk pair: the SAME fixtures, the SAME
//! expected values, with the thunk turned off.
//!
//! `lambda_capture_adapter_tests` asserts that a capturing SAM call site ends
//! up dispatching through a hand-emitted thunk, and that the numbers it
//! produces are right. On its own that is one arm agreeing with one derivation,
//! and a derivation can be wrong in the same direction as the code — the golden
//! values there are computed in Rust, not read off a real JDK.
//!
//! This file runs the same two fixtures with
//! `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0`, which leaves every capturing site on
//! the Rust arm — the path that reads its captures through `heap.get_field` and
//! has been serving them since before any thunk existed. Same expected values.
//! Two independent implementations of "read the captures and call the impl",
//! agreeing.
//!
//! It also asserts the kill switch WORKS: zero capturing thunks installed.
//! Without that this file would pass unchanged if the flag were ignored, and
//! the pair would be one arm run twice.
//!
//! Its own process, because a declared flag latches on first read.
//!
//! **Prerequisites:** Java test classes are compiled automatically by
//! `build.rs` if `javac` is on the PATH. If not, the test is skipped.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::interpreter::lambda_jit_capture_adapter_installs;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn real_jdk_vm() -> Option<Vm> {
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)?;
    let mut config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    config.use_synthetic_jdk = false;
    Some(Vm::new(
        config.with_java_home(java_home.to_string_lossy().into_owned()),
    ))
}

/// Kept in step with `lambda_capture_adapter_tests` by hand rather than shared
/// through a module: an integration test that pulled its expectation from the
/// file it is meant to cross-check would be checking the VM against itself
/// twice, which is exactly the thing this pair exists to avoid.
fn expected_warm_capturing() -> i32 {
    (0..400_000i32).map(|i| ((i & 0xFF) + 7) * 2).sum()
}

fn expected_capture_shapes() -> i32 {
    let mut acc: i64 = 0;
    for i in 0..200_000i64 {
        let x = i & 0xFF;
        acc += x + 4_000_000_029;
        acc += ((x as f64 * 0.25) * 4.0) as i64;
        acc += ((x as f64 * 0.15625) * 64.0) as i64;
        acc += x - 100;
        acc += x + 65_535;
        acc += x - 30_000;
        acc += format!("abcd{x}").len() as i64;
    }
    (acc % 1_000_000_007) as i32
}

fn expected_multi_capture() -> i32 {
    let mut acc: i64 = 0;
    for i in 0..200_000i64 {
        let x = i & 0xFF;
        acc += 1_000_000_007 - 13 * x;
        acc += 3 * 1000 + 4 + x;
        acc += ((0.5 * x as f64 + 41.0) * 2.0) as i64;
    }
    (acc % 1_000_000_007) as i32
}

#[test]
fn the_rust_arm_produces_the_same_numbers_the_thunk_does() {
    if !std::path::Path::new(&format!(
        "{}/cratonvm/LambdaJitTierUp.class",
        test_resources_dir()
    ))
    .exists()
    {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    cratonvm_types::flags::with_process_overrides(
        &[("CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER", Some("0"))],
        run_fixture,
    );
}

fn run_fixture() {
    let Some(mut vm) = real_jdk_vm() else {
        eprintln!(
            "Skipping: needs a class library (CRATONVM_JAVA_HOME / JAVA_HOME / `java` on PATH)."
        );
        return;
    };

    let warm = vm.invoke(
        "cratonvm/LambdaJitTierUp",
        "warmCapturingChecksum",
        "()I",
        &[],
    );
    assert_eq!(
        warm.ok().flatten(),
        Some(Value::Int(expected_warm_capturing()))
    );

    let shapes = vm.invoke(
        "cratonvm/LambdaJitTierUp",
        "captureShapesChecksum",
        "()I",
        &[],
    );
    assert_eq!(
        shapes.ok().flatten(),
        Some(Value::Int(expected_capture_shapes()))
    );

    let multi = vm.invoke(
        "cratonvm/LambdaJitTierUp",
        "multiCaptureChecksum",
        "()I",
        &[],
    );
    assert_eq!(
        multi.ok().flatten(),
        Some(Value::Int(expected_multi_capture()))
    );

    let capture_installs = lambda_jit_capture_adapter_installs();
    assert_eq!(
        capture_installs, 0,
        "CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0 was ignored — {capture_installs} \
         capturing thunk(s) were installed anyway, so this file just ran the \
         same arm `lambda_capture_adapter_tests` runs and the pair proves \
         nothing about agreement between them"
    );
}
