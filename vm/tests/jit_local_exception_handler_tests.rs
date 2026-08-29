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
        && std::path::Path::new(&format!("{dir}/cratonvm/JitSelfRecursiveHandler.class")).exists()
}

/// Every VM in this file, and there is deliberately only one shape of them.
///
/// It carries a REAL class library. `VmConfig::new()` alone does not: it
/// selects `use_synthetic_jdk` (`EMBEDDED_DEFAULT_JDK_MODE`, so the in-tree
/// suite stays hermetic), but the ~5,200 synthetic stubs only exist when the
/// `synthetic-jdk` Cargo feature is compiled in and `cargo test` does not
/// enable it. The default test VM therefore has **neither** library:
/// `String.length()I` and `String.startsWith(Ljava/lang/String;)Z` both raise
/// `NoSuchMethodError`, though both are registered natives — the synthetic
/// `java/lang/String` never declares them, and resolution reads the class, not
/// the registry.
///
/// The `cratonvm` launcher REFUSES that configuration by name ("none of the
/// ~5,200 synthetic stubs are compiled in … a VM with neither the synthetic
/// class library nor a real-JDK boot classpath"). The embedding path has no
/// such guard and hands it over in silence, which is what three tests here
/// were measuring: `throwsInHandlerChecksum` and `LiquibaseScopeBisect`
/// (`String.startsWith`, string-concat `invokedynamic`) and `buildMismatches`
/// (a `StringBuilder` that must survive into the handler). Nothing about the
/// JIT was wrong — with a library the fixtures return their exact golden
/// values, the same numbers a real `java` prints.
///
/// UNIFORM ON PURPOSE. The first repair gave only the three String-using tests
/// a real library and left the rest synthetic; that made
/// `test_jit_rethrow_as_different_type` — untouched, and green before —
/// start failing. Two VMs of different library shapes in ONE test process
/// interfere, so the file uses one shape for all of them.
fn test_vm() -> Vm {
    real_jdk_vm().expect("guarded by require_class_library!")
}

fn real_jdk_vm() -> Option<Vm> {
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)?;
    let mut config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    config.use_synthetic_jdk = false;
    Some(Vm::new(
        config.with_java_home(java_home.to_string_lossy().into_owned()),
    ))
}

/// Skip, loudly and for a stated reason, when no class library can be found.
///
/// NOT a silent `return`: a test that quietly passes on a VM that cannot run
/// its fixture is worse than a red one, because the red one is at least
/// visible. `resolve_java_home_public` is the probe the launcher itself uses.
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

/// `invoke_checksum`'s sibling for the self-recursive fixture, which lives in
/// its own class because its shape needs `java.lang.reflect` and a small class
/// hierarchy rather than another method on `JitLocalHandler`.
fn invoke_self_rec_checksum(method: &str) -> i32 {
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/JitSelfRecursiveHandler", method, "()I", &[]);
    match result {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("{method} returned unexpected value: {other:?}"),
    }
}

#[test]
fn test_jit_self_recursive_activation_catches_its_own_callee_throw() {
    require_class_files!();
    require_class_library!();
    // A compiled frame never dispatches to its own handler — the interpreter's
    // post-return drain does, keyed on the bci `jit_set_throw_bci` stamped.
    // That slot is per-THREAD and carries no activation identity, so in a
    // self-recursive chain the outermost frame's stamp (its own recursive call
    // site, outside the try) overwrote the inner frame's (the real throw site,
    // inside it). The drain then read "outside every protected range", which it
    // treats as a definite "this method cannot catch it", and propagated past a
    // `catch` that covers the throw.
    //
    // Before the fix this test does not return a wrong number — it panics,
    // because the `NoSuchMethodException` escapes `main`. The two tests both
    // routes needed are `route_jit_signal_exception` (the drain) and
    // `run_jit_callee_handler` (the JIT-to-JIT sibling); this fixture exercises
    // both, and fixing only one leaves it throwing.
    //
    // Shape: `org.codehaus.groovy.reflection.stdclasses.CachedSAMClass
    // .hasUsableImplementation`. Golden value from a real JDK.
    assert_eq!(
        invoke_self_rec_checksum("selfRecursiveCalleeThrowChecksum"),
        80_000
    );
}

#[test]
fn test_jit_rethrow_as_different_type() {
    require_class_files!();
    require_class_library!();
    // Mirrors Response.toAbsolute(): try { ... } catch (Inner) { throw new
    // Outer(..., inner) }. The *method containing this try/catch* is called
    // 20000 times directly, so it JIT-compiles and its own handler must
    // fire correctly under compiled code, not just interpreted.
    assert_eq!(invoke_checksum("rethrowChecksum"), 10_039_997);
}

#[test]
fn test_jit_catch_and_return() {
    require_class_files!();
    require_class_library!();
    assert_eq!(invoke_checksum("catchReturnChecksum"), 99_980_000);
}

#[test]
fn test_jit_catch_and_fall_through() {
    require_class_files!();
    require_class_library!();
    assert_eq!(invoke_checksum("catchFallThroughChecksum"), 134_013_267);
}

#[test]
fn test_jit_multi_catch() {
    require_class_files!();
    require_class_library!();
    assert_eq!(invoke_checksum("multiCatchChecksum"), 199_990);
}

#[test]
fn test_jit_nested_try_catch() {
    require_class_files!();
    require_class_library!();
    assert_eq!(invoke_checksum("nestedTryChecksum"), 39_999);
}

#[test]
fn test_jit_exception_in_handler_not_recaught_by_same_handler() {
    require_class_files!();
    require_class_library!();
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
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
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

fn callee_exception_shapes_class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/JitCalleeExceptionShapes.class")).exists()
}

#[test]
fn test_callee_that_cannot_catch_propagates_instead_of_re_running() {
    if !callee_exception_shapes_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    // A compiled callee that DECLARES a handler but whose throw comes from
    // outside every protected range must propagate to its caller. The JIT
    // dispatch helper used to re-execute such a callee from its entry, which
    // runs the prefix a second time — and when the prefix latches state, the
    // re-run takes the early exit and the exception disappears. Golden 20000:
    // every one of 20000 iterations threw exactly once, having run its prefix
    // exactly once. HotSpot 25 agrees.
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/JitCalleeExceptionShapes",
        "outsideTryChecksum",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(v))) => assert_eq!(
            v, 20000,
            "a compiled callee's escaping exception was swallowed or its prefix ran twice \
             — the pc-unknown handler search matches typed rows by exception CLASS ALONE, \
             so a `catch (RuntimeException)` over a range the throw is not in can 'catch' \
             it; see `JitThrowPc::OutsideAllRanges`"
        ),
        other => panic!("outsideTryChecksum returned unexpected value: {other:?}"),
    }
}

#[test]
fn test_two_ranges_one_catch_type_each_reach_their_own_handler() {
    if !callee_exception_shapes_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    // Two disjoint protected ranges catching the SAME type. With the throw pc
    // unknown a class-only match takes the FIRST row whichever range threw,
    // which is how BouncyCastle's `ProvRevocationChecker.check` ran its CRL
    // branch's handler for an OCSP failure and re-issued the query that had
    // just failed instead of falling back. Golden 0.
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/JitCalleeExceptionShapes",
        "twoRangesChecksum",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(v))) => assert_eq!(
            v, 0,
            "a throw reached the handler of the OTHER protected range"
        ),
        other => panic!("twoRangesChecksum returned unexpected value: {other:?}"),
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
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
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

fn precise_handler_frame_class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/JitPreciseHandlerFrame.class")).exists()
}

/// Every entry point returns a MISMATCH COUNT, so a duplicated loop iteration
/// (an OSR bail re-running part of the loop) re-checks instead of corrupting an
/// expected checksum — only a real routing defect makes these non-zero.
fn precise_handler_mismatches(method: &str) -> i32 {
    let mut vm = test_vm();
    match vm.invoke("cratonvm/JitPreciseHandlerFrame", method, "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("{method} returned unexpected value: {other:?}"),
    }
}

#[test]
fn test_compiled_callee_catches_its_own_athrow() {
    if !precise_handler_frame_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // `plainStep`'s handler reads only parameters, so it compiles with or
    // without the precise-handler-frame relaxation. Its callee `maybeThrow`
    // athrows at ITS OWN bci 13, and `JitSignals::athrow_bci` carries no method
    // identity — `plainStep`'s drain used that 13 against its own protected
    // range [0,4), matched no handler, and let the exception escape its own
    // `catch (Boom)`.
    assert_eq!(precise_handler_mismatches("plainMismatches"), 0);
}

#[test]
fn test_precise_handler_frame_catches_a_throw_at_the_end_of_its_try() {
    if !precise_handler_frame_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // `buildStep`'s protected range is [8,14) and its only invoke is at pc 11,
    // so the invoke's SUCCESSOR (14) is `end_pc` — outside the handler. The
    // precise exceptional frame used to be keyed on that successor and handed
    // to `route_jit_exception_through_method` as the THROW pc, so the range
    // test rejected the method's own handler. `sb` (local 2) must also survive
    // into the handler, which is what the precise frame is for.
    assert_eq!(precise_handler_mismatches("buildMismatches"), 0);
}

#[test]
fn test_precise_handler_frame_keeps_a_handler_only_local() {
    if !precise_handler_frame_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // `scopeStep`'s `keep` is read only on the path through the handler, so
    // handler-blind liveness let register allocation alias it with `other`
    // (live across the try).
    assert_eq!(precise_handler_mismatches("scopeMismatches"), 0);
}

#[test]
fn test_compiled_callee_handler_resume_keeps_the_loop_iterator() {
    if !precise_handler_frame_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // `loopStep` is the `BindConverter.convert` shape: the non-parameter local
    // at risk is the loop's own `Iterator`, which the handler never touches —
    // it is read by the loop head the handler falls through to.
    //
    // The exception reaches `run_jit_callee_handler` (the sink that resumes a
    // compiled CALLEE at its own handler, entered only when a COMPILED caller
    // dispatched it — hence `loopCall`), and that sink rebuilt the frame from
    // the incoming arguments alone, ignoring the precise reason-9 frame the
    // compiled body had published. The iterator resumed as null and the next
    // `hasNext()` NPE'd, taking 27 of 43 `LiquibaseAutoConfigurationTests`
    // methods with it.
    //
    // Differential on one binary: `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME=1`
    // ⇒ 19498/20000 mismatches; default ⇒ 0.
    assert_eq!(precise_handler_mismatches("loopMismatches"), 0);
}

#[test]
fn test_an_instanceof_in_a_protected_range_no_longer_refuses_the_method() {
    if !precise_handler_frame_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // `instanceof` (0xc1) sat inside `may_throw_without_precise_frame`'s
    // `0xbb..=0xc1` range, so ANY protected range containing one refused the
    // whole method — even though the x64 lowering of `instanceof` cannot throw
    // (`jit_instanceof` returns 0 or 1 on every path and never stashes a
    // pending exception, and the codegen emits no post-call exception check
    // because there is nothing to check).
    //
    // The refusal is what this test is really about, so assert it directly.
    // `mismatches == 0` ALONE would be a false pass: the interpreter gets this
    // shape right, so a method that never compiles scores a clean zero. That is
    // not hypothetical here — `loopStep`'s first draft had an `instanceof` in
    // its range and read 0 mismatches in both arms of its own A/B for exactly
    // this reason, which is recorded in the fixture's own comment.
    assert_eq!(precise_handler_mismatches("instanceofMismatches"), 0);

    const CLASS: &str = "cratonvm/JitPreciseHandlerFrame";
    const METHOD: &str = "instanceofStep";
    const DESC: &str = "(IZ)I";

    // No refusal was recorded for it at all. Pre-fix this reads
    // `Some("rbc6-handler-reads-unsafe-local(pc=..,op=0xc1)")` — the pc/opcode
    // suffix is what made the cause visible in the first place.
    let reason = cratonvm_jit::jit_bail_reason_for(CLASS, METHOD, DESC);
    assert!(
        !reason
            .as_deref()
            .is_some_and(|r| r.starts_with("rbc6-handler-reads-unsafe-local")),
        "instanceofStep was still refused by the RBC.6 gate: {reason:?}"
    );
    assert!(
        !cratonvm_jit::is_jit_bail_listed(CLASS, METHOD, DESC),
        "instanceofStep was permanently bail-listed; it compiles now"
    );
}

fn osr_loop_progress_class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/JitOsrLoopProgress.class")).exists()
}

/// Every entry point returns a MISMATCH COUNT — a per-iteration visit tally, so
/// an iteration the OSR'd body committed and the interpreter then re-ran is
/// counted directly instead of silently corrupting an expected checksum.
fn osr_loop_progress_mismatches(method: &str) -> i32 {
    let mut vm = test_vm();
    match vm.invoke("cratonvm/JitOsrLoopProgress", method, "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("{method} returned unexpected value: {other:?}"),
    }
}

#[test]
fn test_osr_loop_does_not_rerun_iterations_when_a_callee_catches() {
    if !osr_loop_progress_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // Two defects stacked here, both required for a 0:
    //   * the OSR tier's eager direct-call wiring baked a machine-code CALL
    //     into `step` even though it declares an exception table, so `step`'s
    //     own `catch` never ran and its Boom escaped into the OSR'd caller
    //     (the method-entry tier has had this BUG-H gate all along);
    //   * `try_osr`'s pending-exception drain then "safe rejected" — resuming
    //     the interpreter at the STALE pre-OSR pc, re-running every iteration
    //     the OSR'd body had already committed (20 008 executed for 20 000).
    assert_eq!(osr_loop_progress_mismatches("caughtMismatches"), 0);
}

#[test]
fn test_osr_loop_does_not_rerun_iterations_when_an_exception_escapes() {
    if !osr_loop_progress_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // The pure form: nothing below the caller can catch, so the OSR bail cannot
    // pretend the loop should continue. Before the fix the safe reject resumed
    // it anyway — 42 730 iterations executed where 12 346 were asked for.
    assert_eq!(osr_loop_progress_mismatches("escapeMismatches"), 0);
}

#[test]
fn test_osr_loop_does_not_rerun_iterations_on_an_implicit_npe() {
    if !osr_loop_progress_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // `try_osr`'s pending-NPE drain had the same shape: search this frame's
    // handlers (an OSR'd method provably has none — RBC.6b), then re-stash and
    // resume the loop.
    assert_eq!(osr_loop_progress_mismatches("npeMismatches"), 0);
}

#[test]
fn test_osr_loop_does_not_rerun_iterations_on_an_implicit_aioobe() {
    if !osr_loop_progress_class_files_available() {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    require_class_library!();
    require_class_library!();
    require_class_library!();
    require_class_library!();
    // Sibling of the NPE drain, same defect.
    assert_eq!(osr_loop_progress_mismatches("aioobeMismatches"), 0);
}
