// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `lambda_jit_tierup_tests.rs`, re-run against the OTHER half of the fix.
//!
//! The lambda tier-up has two halves, and they serve different callers. A
//! compiled caller's SAM call is answered by the JIT-side direct arm
//! (`jit::helpers::try_lambda_site_direct_call`); an interpreted caller's is
//! answered by the interpreter's one-shot primitive
//! (`jit_bridge::execute_jit_call_oneshot`). Every fixture method in
//! `LambdaJitTierUp` loops, so every one of them gets its caller OSR-compiled
//! within a few hundred iterations — which means the sibling suite exercises
//! the JIT-side arm and *only* the JIT-side arm.
//!
//! That was measured, not assumed: with a deliberate off-by-one planted in the
//! one-shot's return-value conversion and its implicit-NPE drain deleted, all
//! twelve of those tests still passed.
//!
//! So this file runs the identical fixture, against the identical golden
//! values, with `CRATONVM_JIT_LAMBDA_SITE=0` — which sends a compiled caller's
//! SAM call back down the generic path and therefore through the one-shot.
//! Same numbers, other half. Its own file so the variable is set before any
//! dispatch in the process reads it.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/LambdaJitTierUp.class")).exists()
}

fn real_jdk_vm() -> Option<Vm> {
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)?;
    let mut config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    config.use_synthetic_jdk = false;
    Some(Vm::new(
        config.with_java_home(java_home.to_string_lossy().into_owned()),
    ))
}

macro_rules! require_class_library {
    () => {
        if real_jdk_vm().is_none() {
            eprintln!(
                "Skipping: these fixtures need a class library, and neither a real JDK                  (CRATONVM_JAVA_HOME / JAVA_HOME / `java` on PATH) nor the                  `synthetic-jdk` Cargo feature is available."
            );
            return;
        }
    };
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: .class files not available (javac not on PATH?)");
            return;
        }
    };
}

/// Run the fixture with the JIT-side arm turned off, so the interpreted
/// one-shot is what serves the SAM calls.
///
/// `with_process_overrides`, not `std::env::set_var`: a declared flag is served
/// from a snapshot latched on FIRST read, so setting the variable only takes
/// effect if the call happens to win the race to initialise it — and the VM
/// reads it from threads this test never created, which is what rules out the
/// thread-scoped variant.
///
/// SERIALISED, because process scope is not free: `override_process` swaps one
/// process-wide slot and its docs make serialising against other
/// process-scoped overrides the caller's job — "nesting is supported,
/// concurrent installs are not". `cargo test` runs the twelve tests in this
/// binary in parallel by default, so without this lock two of them install
/// concurrently: the second reads the first's pointer as `previous`, and when
/// they unwind in the other order the restore puts the FIRST override back and
/// leaves it installed for the rest of the process. Nothing crashes (the
/// snapshots are leaked, so the stale pointer stays valid) and the flag is the
/// same value in every test here, which is exactly why it would never be
/// noticed — it would just quietly stop reverting.
fn checksum(method: &str) -> i32 {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    cratonvm_types::flags::with_process_overrides(
        &[("CRATONVM_JIT_LAMBDA_SITE", Some("0"))],
        || {
            let mut vm = real_jdk_vm().expect("guarded by require_class_library!");
            match vm.invoke("cratonvm/LambdaJitTierUp", method, "()I", &[]) {
                Ok(Some(Value::Int(v))) => v,
                other => panic!("{method} returned unexpected value: {other:?}"),
            }
        },
    )
}

// Golden values below were produced by running the byte-identical
// `LambdaJitTierUp.java` fixture under a real JDK (Temurin 25.0.3), driver
// calling each `*Checksum()` method reflectively and printing its result.
//
// Several of them overflow `int`, deliberately: 200 000 iterations of a sum is
// what it takes for these to be testing the compiled path rather than the
// interpreter (see the fixture's own note on `N`), and a wrapped sum is a
// perfectly good checksum — it just has to be the SAME wrapped sum a real JDK
// produces, which is what these are.

#[test]
fn test_oneshot_plain_lambda() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("plainChecksum"), 1_345_494_336);
}

#[test]
fn test_oneshot_capturing_lambda() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("capturingChecksum"), 1_347_894_336);
}

#[test]
fn test_oneshot_method_reference() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("methodRefChecksum"), 1_345_494_336);
}

/// The section 5.3 crash, pinned: a compiled lambda body throwing from a cold
/// branch. Before the one-shot primitive this either crashed the VM outright or
/// returned a wrong sum from the calls that followed the first throw.
#[test]
fn test_oneshot_throwing_lambda_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("throwingChecksum"), -1_620_381_380);
}

#[test]
fn test_oneshot_default_method_through_lambda() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("composedChecksum"), -1_524_778_624);
}

/// Every arm of the one-shot's return-value conversion: `J`, `D`, `L`, and
/// void. A `void` lambda that returns a fabricated zero instead of nothing
/// corrupts the caller's stack; this is the arm that would catch it.
#[test]
fn test_oneshot_return_shapes() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("returnShapesChecksum"), 2_407_060);
}

/// `sig.arithmetic` — divide-by-zero raised inside the compiled body, which
/// reaches the interpreter as an out-of-band signal rather than as a return.
#[test]
fn test_oneshot_arithmetic_exception_from_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("arithmeticChecksum"), 5_266_000);
}

/// `sig.npe` — the same, for an implicit null dereference.
#[test]
fn test_oneshot_npe_from_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("npeChecksum"), 1_210_000);
}

/// `sig.aioobe` — the same, for an out-of-range array index.
#[test]
fn test_oneshot_array_index_exception_from_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("arrayIndexChecksum"), 12_400);
}

/// A body that carries its OWN exception table. The fast path must decline it
/// (every direct-compiled-call site in this VM does), and the interpreted path
/// must still be right.
#[test]
fn test_oneshot_self_catching_body_declines_fast_path() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("selfCatchingChecksum"), -74_309_796);
}

#[test]
fn test_oneshot_nested_lambda_dispatch() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("nestedChecksum"), -1_604_378_624);
}

/// An exception thrown by a warm lambda body and caught two Java frames out:
/// nothing local can route it, so it must propagate out of the one-shot call
/// synchronously, exactly as the interpreted path's own `?` does.
#[test]
fn test_oneshot_exception_propagates_through_two_frames() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("propagationChecksum"), -1_620_381_380);
}
