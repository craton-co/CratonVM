# Bug 11 - `java.io.File.delete()` on a null receiver returned `false`

## Status

**FIXED** on 2026-07-05 in `vm/src/runtime/interpreter.rs`.

## Symptom

`DeployAllServerGroupsTestCase` diverged from HotSpot after its `@BeforeClass`
failed before assigning the static `warFile` field.

HotSpot:

```text
java.lang.NullPointerException: Cannot invoke "java.io.File.delete()" because "...warFile" is null
```

CratonVM before the fix:

```text
java.lang.AssertionError
    at org.junit.Assert.assertTrue(Assert.java:53)
    at org.jboss.as.test.integration.domain.management.cli.DeployAllServerGroupsTestCase.after(DeployAllServerGroupsTestCase.java:56)
```

The class-level result differed too:

```text
CratonVM: found=2 failures=1 errors=1
HotSpot:  found=2 failures=0 errors=2
```

## Root Cause

The interpreter had a compatibility fallback for null receivers on selected
`java/io/File` methods. It returned `false` for boolean methods, including
mutators:

```text
delete
mkdir
mkdirs
```

That violated `invokevirtual` semantics. The VM must null-check the receiver before
dispatch, so `((File) null).delete()` must throw `NullPointerException`, not return
`false`.

The minimized repro showed the exact issue:

```text
warFile is null
delete returned false
```

So the static field was not stale; the null receiver was being tolerated.

## Fix

Removed `delete`, `mkdir`, and `mkdirs` from the `java/io/File` null-receiver fallback.
Those mutating calls now fall through to the normal interpreter null-receiver path and
raise `NullPointerException`.

The older compatibility behavior remains for read-only File probes such as `exists()`
and `isFile()`, which are part of the existing Sonar/Liberty install-root workaround.

## Verification

Build:

```text
cargo check -p cratonvm-vm
cargo build --release -p cratonvm-cli
```

Minimized repro (`StaticWarFileAfterFailure.java`) after the fix, JIT and no-JIT:

```text
warFile is null
after threw java.lang.NullPointerException
java.lang.NullPointerException: Cannot invoke "java.io.File.delete()"
```

WildFly JIT verification:

```text
apps/wildfly-suite-runner/out/verify-deployall-warfile-npe-jit-jit-real-all-20260705-003815/summary.txt
wall=52s
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

WildFly no-JIT verification:

```text
apps/wildfly-suite-runner/out/verify-deployall-warfile-npe-nojit-nojit-real-all-20260705-003917/summary.txt
wall=45s
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

The remaining errors are the expected managed-server startup failure plus teardown NPE
under `MAVEN_ARGS=-Dtimeout.factor=1`, matching the HotSpot baseline shape.

