# Bug 10 - Surefire JUnit4 `Description.createSuiteDescription(String)` linkage probe

## Status

**FIXED** on 2026-07-05 in `native-builtins/src/lib.rs`
(`native_surefire_junit4_reflector_create_description`).

Moved from `docs/known-issues/wildfly-surefire-junit4-description-linkage.md` after
verification.

## Symptom

The WildFly testsuite runner reached Surefire's JUnit4 provider, then CratonVM reported
a linkage failure while Surefire reflected JUnit4 `Description`:

```text
NoSuchMethodError method="org/junit/runner/Description.createSuiteDescription(Ljava/lang/String;)Lorg/junit/runner/Description;"
caller="org/apache/maven/surefire/common/junit4/JUnit4Reflector.createDescription(Ljava/lang/String;)Lorg/junit/runner/Description; @pc=4"
```

Earlier runs treated this as a fork failure before test execution:

```text
The forked VM terminated without properly saying goodbye. VM crash or System.exit called?
Process Exit Code: 1
```

## Root Cause

The WildFly Surefire path uses `surefire-common-junit4-3.5.4` with JUnit 4.13.2.
JUnit 4.13.2 does not expose:

```text
org.junit.runner.Description.createSuiteDescription(String)
```

It exposes the annotation-varargs overload instead:

```text
org.junit.runner.Description.createSuiteDescription(String, Annotation...)
```

Surefire's `JUnit4Reflector.createDescription(String)` intentionally probes the missing
single-String method first, catches `NoSuchMethodError`, and then falls back to the
annotation-varargs overload through reflection. CratonVM's Surefire native path exposed
that probe as a VM-level linkage failure/warning in a place where the provider should
have received the fallback result.

## Fix

CratonVM now registers native implementations for Surefire's reflector helper:

```text
org/apache/maven/surefire/common/junit4/JUnit4Reflector.createDescription(Ljava/lang/String;)Lorg/junit/runner/Description;
org/apache/maven/surefire/common/junit4/JUnit4Reflector.createDescription(Ljava/lang/String;[Ljava/lang/annotation/Annotation;)Lorg/junit/runner/Description;
```

The bridge calls the available `Description.createSuiteDescription(String,
Annotation[])` overload when present, supplying an empty annotation array for the
single-argument helper. If a runtime really has the old single-String overload, it uses
that path.

## Verification

The fixed release binary builds and the one-class WildFly slice reaches real test
execution with the old linkage signature absent from the logs.

Build checks:

```text
cargo check -p cratonvm-native-builtins
cargo build --release -p cratonvm-cli
```

JIT-on verification:

```text
apps/wildfly-suite-runner/out/verify-junit4-reflector-bridge-jit-repeat-jit-real-all-20260705-000051/summary.txt
wall=44s
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

No-JIT verification:

```text
apps/wildfly-suite-runner/out/verify-junit4-reflector-bridge-nojit-nojit-real-all-20260704-235924/summary.txt
wall=39s
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

Targeted log scan for both runs:

```text
NoSuchMethodError: absent
createSuiteDescription linkage warning: absent
JUnit4Reflector linkage warning: absent
```

The remaining test errors are both `RuntimeException: Could not start container`,
caused by `-Dtimeout.factor=1` reducing the WildFly managed-server wait to two seconds.
That confirms this fix removes the Surefire/JUnit4 linkage issue without claiming the
container tests should pass under the shortened timeout.

