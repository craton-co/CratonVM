# Bug 09 - Surefire `CommandReader.awaitStarted()` waits forever in CratonVM fork-shim mode

## Status

**FIXED** on 2026-07-05 in `native-builtins/src/lib.rs`
(`native_surefire_forkedbooter_run`).

## Symptom

The WildFly testsuite runner reached the forked Surefire JVM under CratonVM, parsed the
Surefire provider properties, and then made no forward progress. No test class was
reported before the outer runner killed the class after its timeout.

Representative failed run:

```text
apps/wildfly-suite-runner/out/verify-release-builder-addall-timeout1-nojit-nojit-real-all-20260702-195510/results.tsv
org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase  TIMEOUT  ...  wall ~= 313s
```

The same hang reproduced with `--jit off`, so it was not a JIT codegen issue.

## Root Cause

CratonVM's Surefire fork shim drives `ForkedBooter` through native code: it calls
`setupBooter(...)`, then invokes `execute(...)` in-process for the one selected test
class. In this mode the test set is already materialized by `setupBooter`, but
Surefire 3.5.4 still leaves `ForkedBooter.commandReader` live.

When the JUnit4 provider is constructed with a non-null command reader, it can wait in
the fork command path (`CommandReader.awaitStarted()`). The WildFly runner is not
running an interactive Surefire master command stream for these single-class fork-shim
invocations, so the provider waits forever.

## Fix

After `setupBooter(...)` succeeds and before `execute(...)` is invoked, CratonVM now
sets `ForkedBooter.commandReader` to `null`. This matches the runner mode: the selected
test class is already known, and there is no external command reader that can deliver a
later "start" command.

Code location:

```text
native-builtins/src/lib.rs
  native_surefire_forkedbooter_run
```

## Verification

All verification runs used `CRATONVM_BIN=C:\craton\cratonvm\target\release\cratonvm.exe`
and `MAVEN_ARGS=-Dtimeout.factor=1` to keep the container-start failure short.

Before the fix:

```text
apps/wildfly-suite-runner/out/verify-release-builder-addall-timeout1-nojit-nojit-real-all-20260702-195510/results.tsv
TIMEOUT, 0 test methods, external wall ~= 313s
```

After the fix, no JIT:

```text
apps/wildfly-suite-runner/out/verify-junit4-reflector-bridge-nojit-nojit-real-all-20260704-235924/summary.txt
wall=39s
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

After the fix, JIT on:

```text
apps/wildfly-suite-runner/out/verify-junit4-reflector-bridge-jit-repeat-jit-real-all-20260705-000051/summary.txt
wall=44s
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

The remaining `FAIL` is the expected WildFly managed-server startup timeout from the
deliberately tiny `-Dtimeout.factor=1`, not a Surefire hang. The fork also logs:

```text
[SUREFIRE-ACK-EXIT] invoked; eventChannel_null=false commandReader_null=true
```

