// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Proof that a SAM call site really does end up dispatching in machine code.
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
fn test_inline_cache_takes_over_the_sam_call_site() {
    if !std::path::Path::new(&format!(
        "{}/cratonvm/LambdaJitTierUp.class",
        test_resources_dir()
    ))
    .exists()
    {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }

    // `with_process_overrides`, not `std::env::set_var`: a declared flag is
    // served from a snapshot latched on FIRST read, so setting the variable
    // only takes effect if the call wins the race to initialise it — and the
    // VM reads these from threads this test never created, which rules out
    // the thread-scoped variant.
    //
    // NOT `CRATONVM_BG_COMPILE=0`. Inline compilation looks like the way to
    // make "the body is compiled by iteration N" deterministic, and it is —
    // but measured across this fixture it compiles about 6% of what the
    // background worker does, because the inline `try_jit_upgrade_with_gate`
    // route declines bodies the worker admits. Determinism bought by
    // suppressing the thing under test is not determinism.
    cratonvm_types::flags::with_process_overrides(
        &[("CRATONVM_DBG_LAMBDA_JIT", Some("1"))],
        || run_fixture(),
    );
}

fn run_fixture() {
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
    // The thunk is installed per SITE, so this counts sites and not calls —
    // which is the point. Every dispatch after one lands never reaches Rust
    // again, so a per-call counter necessarily goes quiet exactly when the
    // feature starts working, and this is the number that does not.
    let installs = cratonvm_vm::runtime::interpreter::lambda_jit_adapter_installs();
    assert!(
        installs > 0,
        "no SAM call site was given an inline-cache thunk (site_direct={site_direct},          fast_returns={fast_returns}) — the call site is still answered by Rust on          every call, which is the whole defect this was built to remove"
    );
    // And the corollary, which is the actual claim: Rust is no longer ON the
    // path. A handful of calls precede the install; hundreds of thousands do
    // not follow it.
    assert!(
        site_direct < 10_000,
        "{site_direct} of 800 000 dispatches still went through the Rust arm          after {installs} thunk install(s) — the inline cache is not serving the          call site it was installed into"
    );
}
