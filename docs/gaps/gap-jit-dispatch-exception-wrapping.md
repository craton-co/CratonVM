# Gap: JIT dispatch wraps exceptions as `InternalError`

**Discovered:** 2026-06-09 (app test suite run — `commons-math-junit-probe`)  
**Severity:** High — any exception thrown inside a JIT-compiled method gets re-wrapped as `InternalError` instead of propagating to the caller. Any code that expects to catch `NoClassDefFoundError`, `NullPointerException`, etc. from a JIT-compiled callee will instead receive `InternalError` and break.  
**Status:** **FIXED 2026-06-09** — (1) primary wrapping bug resolved in `vm/src/jit/helpers.rs::handle_jit_dispatch_error`; (2) secondary `TransformUtilsTest` "classloader gap" resolved in `test-infra/run-all-apps-suites.sh` and `test-infra/run-comparison-full.sh` (not a VM bug — missing transitive test deps in the suite-runner classpath, see [§ Secondary finding](#secondary-finding-transformutilstest-class-not-found)).

---

## Symptom

```
Exception in thread "main" java/lang/InternalError: JIT dispatch into
  org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute(
    Lorg/junit/platform/launcher/core/LauncherDiscoveryResult;
    Lorg/junit/platform/engine/EngineExecutionListener;)V
  failed: linkage error: no class def found: org/apache/commons/math4/transform/TransformUtilsTest
    at JUnitProbe.main(JUnitProbe.java:22)
    at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(...)
    at org/junit/platform/launcher/core/EngineExecutionOrchestrator.withInterceptedStreams(...)
```

**Expected behavior:** `NoClassDefFoundError` thrown inside `EngineExecutionOrchestrator.execute` should propagate normally to the caller, which catches it and handles it (JUnit Platform's test execution error handling).

**Actual behavior:** The JIT dispatch layer intercepts the exception and wraps it in `java.lang.InternalError`, re-throwing from the call site in `JUnitProbe.main`. `JUnitProbe` doesn't catch `InternalError`, so the JVM terminates.

---

## Reproduction

```bash
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
CV="C:/craton/CratonVM/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
JUNIT="C:/craton/CratonVM/.bench-cache/junit-platform-console-standalone-1.10.2.jar"
CM="C:/craton/CratonVM/apps/_test-suites/commons-math"
M2="C:/Users/Victor/.m2/repository"
CP="$JUNIT;$CM/commons-math-transform/target/classes;$CM/commons-math-transform/target/test-classes"
CP="$CP;$CM/commons-math-core/target/classes"
CP="$CP;$M2/org/apache/commons/commons-numbers-rng/1.2/commons-numbers-rng-1.2.jar"
CP="$CP;$M2/org/apache/commons/commons-numbers-core/1.2/commons-numbers-core-1.2.jar"

"$CV" --java-home "$JDK" --Xmx 2g -cp "C:/craton/CratonVM/bench;$CP" JUnitProbe 2>&1
```

The test discovery succeeds (DISCOVER_FOUND=4, all 4 `TransformUtilsTest` methods found). The error fires when JUnit Platform begins executing the first test.

---

## Root cause analysis

The JIT compiled `EngineExecutionOrchestrator.execute`. When this method (or something it calls) attempts to reference `TransformUtilsTest` — during first-time class initialization triggered by test execution — CratonVM cannot find the class definition and throws `NoClassDefFoundError`.

The JIT dispatch mechanism (`invoke_dispatch` or equivalent in `vm/src/jit/`) catches the exception from the JIT-compiled frame and instead of returning it as a normal Java exception to the interpreter, wraps it in `InternalError`.

**Why it wraps:** The JIT dispatch path likely has an error handler that calls something like:
```rust
Err(e) => Err(InternalError::new(format!("JIT dispatch into {} failed: {}", method, e)))
```
This is appropriate for *JIT compilation errors* or *ABI mismatches* but must NOT be used for runtime Java exceptions thrown by the compiled code. A `NoClassDefFoundError` during execution is a normal Java exception — it must be re-thrown to the Java exception table, not wrapped.

---

## Fix landed

`handle_jit_dispatch_error` in [vm/src/jit/helpers.rs](../../vm/src/jit/helpers.rs) (the same handler that already routes `VmError::Runtime` → real Java exception, mirroring the interpreter's per-instruction post-processing) was extended with two new match arms before the catch-all `InternalError` wrap:

1. **`VmError::Linkage(linkage_err)`** — every variant of `LinkageError` is now mapped to the matching `java.lang.*` throwable (`NoClassDefFoundError`, `NoSuchFieldError`, `NoSuchMethodError`, `IncompatibleClassChangeError`, `AbstractMethodError`, `IllegalAccessError`, `VerifyError`, `ClassFormatError`, `UnsupportedOperationException`). The detail message is composed from the variant's fields (e.g. `Class.method(desc)` for `NoSuchMethodError`).
2. **`VmError::ClassFile(ClassNotFound { class_name })`** — mapped to `java/lang/NoClassDefFoundError`, mirroring the interpreter's `raise_no_class_def_found` boundary helper.

Verified by re-running the repro: now reports `Exception in thread "main" java/lang/NoClassDefFoundError: org/apache/commons/math4/transform/TransformUtilsTest`. All 64 JIT unit tests still pass.

## Fix direction (original analysis)

In the JIT dispatch handler (likely `vm/src/jit/x64.rs` or `vm/src/jit/dispatch.rs`), find the code path that handles exceptions from JIT-compiled frames. There should be a distinction between:

1. **JIT infrastructure failure** (can't enter JIT frame, ABI error, JIT code corrupt): wrap in `InternalError` — this is appropriate.
2. **Java exception thrown by JIT-compiled code** (NPE, NCDFE, etc.): propagate as-is — this is the bug.

The fix: check if the error returned from the JIT dispatch is a Java exception object (already thrown by bytecode logic) and if so, re-throw it as-is without wrapping.

Pseudocode of the fix:
```rust
match jit_dispatch(frame, method, args) {
    Ok(result) => Ok(result),
    Err(JvmError::JavaException(e)) => Err(JvmError::JavaException(e)),  // propagate unchanged
    Err(e) => Err(JvmError::JavaException(
        InternalError::new(format!("JIT dispatch failed: {}", e))  // only for JIT infra errors
    )),
}
```

---

## Secondary finding: `TransformUtilsTest` class not found

**RESOLVED 2026-06-09** — not a VM bug; missing transitive test dependency in the suite-runner classpath.

Once the primary wrapping bug was fixed and the underlying error became visible, `CRATONVM_DBG_NCDFE=1` showed the *real* missing class was `org/apache/commons/math3/analysis/function/Sin`, referenced from `TransformUtilsTest.<clinit>` (a static field). The discover phase only reads annotations and never triggers `<clinit>`, so the class-init failure was invisible until execute tried to instantiate the test class — at which point the JVM marked `TransformUtilsTest` as erroneous (JVMS §5.5) and surfaced the failure as `NoClassDefFoundError` on the *test class*, masking the actual missing dependency.

The suite-runner classpath in [test-infra/run-all-apps-suites.sh](../../test-infra/run-all-apps-suites.sh) was missing several transitive test deps declared in [commons-math-transform/pom.xml](../../apps/_test-suites/commons-math/commons-math-transform/pom.xml): `commons-math3`, `commons-rng-simple` (+ its `client-api` / `core` siblings), `commons-numbers-complex`. Adding them — with `[ -f $jar ]` guards so missing jars are skipped, not fatal — yields `EXEC_FOUND=4 SUCCEEDED=4 FAILED=0` on CratonVM, both with and without JIT. The same fix was applied to [test-infra/run-comparison-full.sh](../../test-infra/run-comparison-full.sh).

**Diagnostic that proved this:** run with `CRATONVM_DBG_NCDFE=1`; the printed `[NCDFE]` line names the *actual* missing class, and the `[NCDFE-STK]` frames show which `<clinit>` triggered the resolve. Use this any time a `NoClassDefFoundError` on a test class looks like a "VM can't reload the class" classloader gap — it's usually a transitive dep missing from the runner classpath.

---

## Impact scope

Any code that:
- Calls a JIT-compiled method that throws any Java exception
- Expects to catch that exception at the call site

is broken. In practice this affects any sufficiently-loaded application where hot methods get JIT-compiled. The bug may be latent in many code paths where exceptions are expected to propagate but are currently being masked as `InternalError` (and silently crashing instead of being handled).
