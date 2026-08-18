// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A lambda SAM implementation is now counted for JIT tier-up and, once
//! compiled, entered through a DIRECT compiled call — see
//! `known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md`
//! and `jit_bridge::execute_jit_call_oneshot`.
//!
//! Direct compiled entry leaves the interpreter entirely, so everything the
//! interpreted frame path did for free — converting the return value,
//! propagating an exception, routing an implicit NPE/AIOOBE/divide-by-zero,
//! recovering from a deoptimization — has to be reproduced by hand. Each test
//! here is one of those, and each one's golden value came from running the
//! byte-identical `LambdaJitTierUp.java` fixture under a real JDK (`java`).
//!
//! The exception arms are the point. A lambda body whose `throw` sits on a cold
//! branch compiles that branch to an uncommon trap, so the FIRST throwing call
//! after compilation deoptimizes — and reusing the interpreter's own
//! loop-integrated `execute_jit_call_decoded` for this one-shot dispatch left
//! that deopt's resumed frame ORPHANED on the thread, which surfaced thousands
//! of calls later as a `usize::MAX` operand-stack underflow inside an unrelated
//! interpreted run of the same body. `test_throwing_lambda_body` and
//! `test_exception_propagates_through_two_frames` are that crash, pinned.
//!
//! **Prerequisites:** Java test classes are compiled automatically by
//! `build.rs` if `javac` is on the PATH. If not, tests are skipped.

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

/// A VM with a REAL class library — the fixture is built out of
/// `java.util.function`, which the synthetic library does not carry. Same
/// shape, and the same reasoning, as `jit_local_exception_handler_tests.rs`:
/// one library shape for every VM in the file.
fn real_jdk_vm() -> Option<Vm> {
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)?;
    let mut config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    config.use_synthetic_jdk = false;
    Some(Vm::new(
        config.with_java_home(java_home.to_string_lossy().into_owned()),
    ))
}

/// Skip, loudly and for a stated reason, when no class library can be found.
macro_rules! require_class_library {
    () => {
        if real_jdk_vm().is_none() {
            eprintln!(
                "Skipping: these fixtures need a class library, and neither a real JDK \
                 (CRATONVM_JAVA_HOME / JAVA_HOME / `java` on PATH) nor the \
                 `synthetic-jdk` Cargo feature is available."
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

fn checksum(method: &str) -> i32 {
    let mut vm = real_jdk_vm().expect("guarded by require_class_library!");
    match vm.invoke("cratonvm/LambdaJitTierUp", method, "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("{method} returned unexpected value: {other:?}"),
    }
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
fn test_plain_lambda() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("plainChecksum"), 1_345_494_336);
}

#[test]
fn test_capturing_lambda() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("capturingChecksum"), 1_347_894_336);
}

#[test]
fn test_method_reference() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("methodRefChecksum"), 1_345_494_336);
}

/// The section 5.3 crash, pinned: a compiled lambda body throwing from a cold
/// branch. Before the one-shot primitive this either crashed the VM outright or
/// returned a wrong sum from the calls that followed the first throw.
#[test]
fn test_throwing_lambda_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("throwingChecksum"), -1_620_381_380);
}

#[test]
fn test_default_method_through_lambda() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("composedChecksum"), -1_524_778_624);
}

/// Every arm of the one-shot's return-value conversion: `J`, `D`, `L`, and
/// void. A `void` lambda that returns a fabricated zero instead of nothing
/// corrupts the caller's stack; this is the arm that would catch it.
#[test]
fn test_return_shapes() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("returnShapesChecksum"), 2_407_060);
}

/// `sig.arithmetic` — divide-by-zero raised inside the compiled body, which
/// reaches the interpreter as an out-of-band signal rather than as a return.
#[test]
fn test_arithmetic_exception_from_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("arithmeticChecksum"), 5_266_000);
}

/// `sig.npe` — the same, for an implicit null dereference.
#[test]
fn test_npe_from_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("npeChecksum"), 1_210_000);
}

/// `sig.aioobe` — the same, for an out-of-range array index.
#[test]
fn test_array_index_exception_from_body() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("arrayIndexChecksum"), 12_400);
}

/// A body that carries its OWN exception table. The fast path must decline it
/// (every direct-compiled-call site in this VM does), and the interpreted path
/// must still be right.
#[test]
fn test_self_catching_body_declines_fast_path() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("selfCatchingChecksum"), -74_309_796);
}

#[test]
fn test_nested_lambda_dispatch() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("nestedChecksum"), -1_604_378_624);
}

/// An exception thrown by a warm lambda body and caught two Java frames out:
/// nothing local can route it, so it must propagate out of the one-shot call
/// synchronously, exactly as the interpreted path's own `?` does.
#[test]
fn test_exception_propagates_through_two_frames() {
    require_class_files!();
    require_class_library!();
    assert_eq!(checksum("propagationChecksum"), -1_620_381_380);
}
