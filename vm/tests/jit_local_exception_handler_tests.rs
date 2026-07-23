// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RBC.6 regression: JIT-compiled methods with a LOCAL exception handler
//! (a non-empty exception table on the method that also contains, or whose
//! own call site triggers, a throw covered by that table) must dispatch to
//! their own handler correctly once JIT-compiled, not just when
//! interpreted.
//!
//! Before this fix, `try_compile_inner` (jit/src/lib.rs) permanently
//! refused to compile any method combining `athrow` with a non-empty local
//! exception table. Each `*Step` method in `JitLocalHandler.java` is called
//! 20000 times directly (well past the default JIT hotness threshold of
//! 500), so the method containing the try/catch is itself JIT-compiled and
//! its own handler dispatch runs through compiled code for the bulk of the
//! iterations. Expected checksums were computed by running the identical
//! fixture under a real JDK (`java`) — see the Java source's own doc
//! comment for the exact shapes covered (rethrow-as-different-type mirrors
//! `org.apache.catalina.connector.Response.toAbsolute()`; catch+return;
//! catch+fall-through; multi-catch; nested try/catch; exception thrown
//! inside a handler must not be re-caught by that same handler).
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
    std::path::Path::new(&format!("{dir}/cratonvm/JitLocalHandler.class")).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: .class files not available (javac not on PATH?)");
            return;
        }
    };
}

fn invoke_checksum(method: &str) -> i32 {
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/JitLocalHandler", method, "()I", &[]);
    match result {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("{method} returned unexpected value: {other:?}"),
    }
}

// Golden values below were computed by running the byte-identical
// `JitLocalHandler.java` fixture under a real JDK (`java`), package
// `cratonvm`, driver printing each `*Checksum()` method's return value.

#[test]
fn test_jit_rethrow_as_different_type() {
    require_class_files!();
    // Mirrors Response.toAbsolute(): try { ... } catch (Inner) { throw new
    // Outer(..., inner) }. The *method containing this try/catch* is called
    // 20000 times directly, so it JIT-compiles and its own handler must
    // fire correctly under compiled code, not just interpreted.
    assert_eq!(invoke_checksum("rethrowChecksum"), 10_039_997);
}

#[test]
fn test_jit_catch_and_return() {
    require_class_files!();
    assert_eq!(invoke_checksum("catchReturnChecksum"), 99_980_000);
}

#[test]
fn test_jit_catch_and_fall_through() {
    require_class_files!();
    assert_eq!(invoke_checksum("catchFallThroughChecksum"), 134_013_267);
}

#[test]
fn test_jit_multi_catch() {
    require_class_files!();
    assert_eq!(invoke_checksum("multiCatchChecksum"), 199_990);
}

#[test]
fn test_jit_nested_try_catch() {
    require_class_files!();
    assert_eq!(invoke_checksum("nestedTryChecksum"), 39_999);
}

#[test]
fn test_jit_exception_in_handler_not_recaught_by_same_handler() {
    require_class_files!();
    // Encoded as checksum*10000 + secondaryEscapes (see the Java source).
    // secondaryEscapes must be exactly 2000 (one per x==7 hit, i in
    // 0..20000 stepping x=i%10 => 2000 hits) — if the JIT's routing ever
    // misrouted the handler's own throw back into itself instead of
    // propagating past the method, this would diverge (a different
    // checksum, a hang, or a stack overflow).
    assert_eq!(invoke_checksum("throwsInHandlerChecksum"), 900_002_000);
}

fn athrow_bisect_class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/AthrowCountBisect.class")).exists()
}

#[test]
fn test_jit_two_sequential_try_catch_blocks_same_method() {
    if !athrow_bisect_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    // Bisection case: TWO separate, sequential (non-nested) try/catch blocks
    // in one method, first catching RuntimeException, second catching
    // IllegalStateException (a RuntimeException subclass). Golden (real
    // JDK) value is 19998 — computed independently, see
    // AthrowCountBisect.java's doc comment for the derivation.
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AthrowCountBisect",
        "twoThrowsChecksum",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(v))) => assert_eq!(
            v, 19998,
            "two-sequential-try/catch checksum diverged from the real-JDK golden value \
             (19998) — likely the `route_jit_exception_through_method`/`bail_to_interpreter` \
             throw_pc=usize::MAX type-only handler matching misrouting an exception to an \
             earlier, unrelated try/catch entry whose catch type is a supertype of the one \
             actually thrown (IllegalStateException is-a RuntimeException)"
        ),
        other => panic!("twoThrowsChecksum returned unexpected value: {other:?}"),
    }
}

fn liquibase_scope_bisect_class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/LiquibaseScopeBisect.class")).exists()
}

#[test]
fn test_jit_indy_after_side_effect_no_double_execution() {
    // BUG-LQB-SCOPE regression (see docs/feature-designs/jit-local-exception-handlers.md,
    // "session 2"): a static counter mutation immediately followed by a
    // string-concat `invokedynamic`, mirroring the original Liquibase
    // `Scope.enter()` corruption shape as closely as possible. Before the
    // BUG-LQB-SCOPE gate relaxation this method could not have hit the bug
    // (RBC.6 blocked compilation of anything with a local exception
    // handler, and this method has none — it never needed RBC.6 at all).
    // It IS, however, exactly the shape the original `752796a0a` fix
    // targeted: `jit_scan`'s `0xba` arm used to refuse to compile ANY
    // method with an `invokedynamic` preceded by a committing side effect
    // in raw pc order, believing the trap's fallback re-ran the whole
    // method from entry (double-executing the counter increment). That
    // premise died the same day a concurrent fix made the trap's
    // precise-resume routing unconditional — this test's counter must be
    // exactly 3,000,000 after 3,000,000 calls, not ~6,000,000.
    if !liquibase_scope_bisect_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/LiquibaseScopeBisect", "checksum", "()I", &[]);
    match result {
        Ok(Some(Value::Int(_))) => {}
        other => panic!("checksum returned unexpected value: {other:?}"),
    }
    let counter_after = vm.invoke("cratonvm/LiquibaseScopeBisect", "counterValue", "()I", &[]);
    match counter_after {
        Ok(Some(Value::Int(v))) => assert_eq!(
            v, 3_000_000,
            "counter after 3,000,000 calls diverged from 3,000,000 — the JIT double-executed              the side effect preceding the invokedynamic trap (the exact BUG-LQB-SCOPE shape)"
        ),
        other => panic!("counterValue returned unexpected value: {other:?}"),
    }
}
