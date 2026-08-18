// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Proof that `lambda_jit_oneshot_tests` is testing something.
//!
//! Those tests assert that a lambda-dense fixture produces the same numbers
//! CratonVM and a real JDK both produce. Every one of them would ALSO pass on a
//! VM where the lambda tier-up fast path never engaged — the interpreted path
//! computes the same answers, which is the whole point of a fast path. A green
//! suite there would be agreement about a code path nobody took.
//!
//! So this file asserts the path was taken. It runs a fixture method long
//! enough (400 000 dispatches) that the background compiler certainly publishes
//! the impl body, and then reads the engagement counters
//! (`lambda_jit_engagement`, gated on `CRATONVM_DBG_LAMBDA_JIT` — set below
//! BEFORE any VM exists, because the gate is read once into a `OnceLock`).
//!
//! Its own file, and therefore its own process: the gate cannot be set after
//! another test in the same binary has already dispatched a lambda and locked
//! it to `false`.
//!
//! **Prerequisites:** Java test classes are compiled automatically by
//! `build.rs` if `javac` is on the PATH. If not, the test is skipped.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::interpreter::lambda_jit_engagement;
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

#[test]
fn test_lambda_one_shot_actually_engages() {
    if !std::path::Path::new(&format!(
        "{}/cratonvm/LambdaJitTierUp.class",
        test_resources_dir()
    ))
    .exists()
    {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    // Before the first dispatch: the counters' gate is a `OnceLock`.
    // SAFETY: single-threaded, and the first statement of the only test in this
    // binary — nothing else can be reading the environment concurrently.
    std::env::set_var("CRATONVM_DBG_LAMBDA_JIT", "1");
    // Compile on the mutator at the threshold rather than racing a worker, so
    // "the body is compiled well before 800 000 dispatches are done" is a fact.
    // SAFETY: as above.
    std::env::set_var("CRATONVM_BG_COMPILE", "0");
    // ...and the JIT-side arm OFF, so the interpreted one-shot is what serves.
    // SAFETY: as above.
    std::env::set_var("CRATONVM_JIT_LAMBDA_SITE", "0");

    let Some(mut vm) = real_jdk_vm() else {
        eprintln!(
            "Skipping: needs a class library (CRATONVM_JAVA_HOME / JAVA_HOME / `java` on PATH)."
        );
        return;
    };
    let result = vm.invoke("cratonvm/LambdaJitTierUp", "warmChecksum", "()I", &[]);
    // 800 000 dispatches: 400 000 iterations x two call shapes.
    // Two calls per iteration — one through the compiled `step` hop, one
    // direct — so both call shapes are exercised.
    let expected: i32 = (0..400_000i32).map(|i| ((i & 0xFF) + 3) * 2).sum();
    assert_eq!(result.ok().flatten(), Some(Value::Int(expected)));

    let (fast_returns, site_direct, nominations) = lambda_jit_engagement();
    // NOT asserted: that the tier-up counter is what nominated this body.
    // Measured, it usually is not — `step`'s own compilation, or the eager
    // first-call compile, gets there first, and the counter's contribution is
    // the bodies NOTHING else nominates. `nominations` is reported in the
    // messages below rather than asserted on, because an assertion on it would
    // fail on a run where the feature worked.
    let _ = nominations;
    assert!(
        fast_returns + site_direct > 0,
        "the impl was nominated ({nominations} times) but no call ever entered a \
         compiled body — every correctness test in lambda_jit_tierup_tests is \
         therefore green about a path it never took"
    );
    // And specifically the INTERPRETED arm, the half this configuration
    // (`CRATONVM_JIT_LAMBDA_SITE=0`) exists to cover — the mirror of the
    // assertion in `lambda_jit_engagement_tests`.
    assert!(
        fast_returns > 100_000,
        "only {fast_returns} of 800 000 dispatches took the interpreted one-shot          (the JIT-side arm took {site_direct}) — lambda_jit_oneshot_tests would          then be testing the JIT-side arm twice and the one-shot not at all"
    );
}
