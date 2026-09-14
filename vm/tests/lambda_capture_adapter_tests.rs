// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Proof that a CAPTURING SAM call site dispatches in machine code, and that
//! the thunk reads the right bytes when it does.
//!
//! `lambda_jit_adapter_engagement_tests` asserts that a call site gets an
//! inline-cache thunk. It cannot say anything about capturing lambdas: its
//! fixture captures nothing, so its counter stays healthy whatever the capture
//! path does. This file asserts the narrower fact —
//! `lambda_jit_capture_adapter_installs() > 0` — and then checks the numbers
//! that only a correct set of loads can produce.
//!
//! The paired file `lambda_capture_adapter_off_tests` runs the same two
//! fixtures with `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0`, i.e. on the Rust arm,
//! and asserts the SAME expected values. Neither file alone is worth much: this
//! one could agree with a broken Rust arm, and that one could pass while the
//! thunk was never built. Together they say the two arms agree and that both
//! ran.
//!
//! Its own file, and therefore its own process, because a declared flag latches
//! on first read and the census gate is a `OnceLock`.
//!
//! **Prerequisites:** Java test classes are compiled automatically by
//! `build.rs` if `javac` is on the PATH. If not, the test is skipped.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::interpreter::{
    lambda_jit_adapter_installs, lambda_jit_capture_adapter_installs, lambda_jit_engagement,
};
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

fn fixture_present() -> bool {
    std::path::Path::new(&format!(
        "{}/cratonvm/LambdaJitTierUp.class",
        test_resources_dir()
    ))
    .exists()
}

/// `warmCapturingChecksum`: 400 000 iterations, two calls each, each returning
/// `(i & 0xFF) + 7`.
pub fn expected_warm_capturing() -> i32 {
    (0..400_000i32).map(|i| ((i & 0xFF) + 7) * 2).sum()
}

/// `captureShapesChecksum`, arm for arm.
///
/// Written as the same loop rather than a closed form on purpose: a closed form
/// is a second derivation that can be wrong in a way that happens to match a
/// wrong VM, and the arms here are chosen precisely because each has a distinct
/// failure value (156 for a byte read unsigned, a negative for a char read
/// signed, a truncated `long`).
pub fn expected_capture_shapes() -> i32 {
    let mut acc: i64 = 0;
    for i in 0..200_000i64 {
        let x = i & 0xFF;
        // `long` capture: wide payload, does not fit in 32 bits.
        acc += x + 4_000_000_029;
        // `double` capture, scaled back by an exact power of two.
        acc += ((x as f64 * 0.25) * 4.0) as i64;
        // `float` capture: 0.15625 * 64 == 10, exactly.
        acc += ((x as f64 * 0.15625) * 64.0) as i64;
        // `byte` capture of -100 — 156 if the load does not sign-extend.
        acc += x - 100;
        // `char` capture of 0xFFFF — negative if the load sign-extends the
        // 16-bit value rather than reading the stored `int`.
        acc += x + 65_535;
        // `short` capture, negative.
        acc += x - 30_000;
        // reference capture: "abcd" plus the decimal digits of x.
        acc += format!("abcd{x}").len() as i64;
    }
    (acc % 1_000_000_007) as i32
}

/// `multiCaptureChecksum`: the arm that can see the capture INDEX at all.
///
/// A thunk that read every capture from cell 0 passed both functions above and
/// the engagement assertions with them — measured, by planting exactly that
/// break — because every lambda in those two fixtures captures exactly one
/// value. These three capture two each, combined non-commutatively.
pub fn expected_multi_capture() -> i32 {
    let mut acc: i64 = 0;
    for i in 0..200_000i64 {
        let x = i & 0xFF;
        // Two `long` captures, same width: isolates the index from the load.
        acc += 1_000_000_007 - 13 * x;
        // `int` + reference: "wxyz".length() is 4.
        acc += 3 * 1000 + 4 + x;
        // `double` + `int`: wide cell then narrow, so a collapsed index also
        // picks the wrong load.
        acc += ((0.5 * x as f64 + 41.0) * 2.0) as i64;
    }
    (acc % 1_000_000_007) as i32
}

#[test]
fn a_capturing_sam_call_site_dispatches_through_a_thunk() {
    if !fixture_present() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    cratonvm_types::flags::with_process_overrides(
        &[("CRATONVM_DBG_LAMBDA_JIT", Some("1"))],
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
        Some(Value::Int(expected_warm_capturing())),
        "a capturing lambda dispatched through a thunk returned the wrong sum — \
         the capture is read at a fixed cell offset, so a wrong one is a wrong \
         number here rather than a crash"
    );

    let shapes = vm.invoke(
        "cratonvm/LambdaJitTierUp",
        "captureShapesChecksum",
        "()I",
        &[],
    );
    assert_eq!(
        shapes.ok().flatten(),
        Some(Value::Int(expected_capture_shapes())),
        "one of the capture WIDTHS is loaded wrongly — see the per-arm comments \
         in `expected_capture_shapes` for which failure each arm names"
    );

    let multi = vm.invoke(
        "cratonvm/LambdaJitTierUp",
        "multiCaptureChecksum",
        "()I",
        &[],
    );
    assert_eq!(
        multi.ok().flatten(),
        Some(Value::Int(expected_multi_capture())),
        "the capture INDEX is wrong — each of these lambdas holds two captures \
         and combines them non-commutatively, so this is the arm that sees a \
         thunk reading them from one cell, or in the wrong order"
    );

    let (fast_returns, site_direct, _nominations) = lambda_jit_engagement();
    let installs = lambda_jit_adapter_installs();
    let capture_installs = lambda_jit_capture_adapter_installs();
    assert!(
        capture_installs > 0,
        "no CAPTURING SAM call site was given an inline-cache thunk \
         ({installs} thunk install(s) in total, all non-capturing; \
         site_direct={site_direct} fast_returns={fast_returns}) — both assertions \
         above are therefore agreement about a path nothing took, and every \
         capturing lambda is still answered by Rust on every call"
    );
    // Rust is off the path, not merely joined by a thunk. A handful of calls
    // precede each install; the hundreds of thousands after it do not follow.
    assert!(
        site_direct < 20_000,
        "{site_direct} dispatches still went through the Rust arm after \
         {capture_installs} capturing thunk install(s) — the inline cache is not \
         serving the call sites it was installed into"
    );
}
