# Keycloak empty-stderr process exits

Status: open

Date observed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 2 `CRASH`
rows with process return code `-1` and no captured stdout/stderr content:

```text
tests/base org.keycloak.tests.admin.client.ClientProtocolMapperTest
tests/base org.keycloak.tests.admin.client.ClientProtocolValidationTest
```

Unlike the other crash buckets, these rows do not include a CratonVM linkage
warning or JUnit summary. The runner recorded both as `CRASH` because the process
exited abnormally and emitted no recognizable result marker.

## Representative Rows

```text
index=52
module=tests/base
class=org.keycloak.tests.admin.client.ClientProtocolMapperTest
status=CRASH
rc=-1
seconds=6.690
stdoutLog=...ClientProtocolMapperTest.out.log
stderrLog=...ClientProtocolMapperTest.err.log
```

```text
index=53
module=tests/base
class=org.keycloak.tests.admin.client.ClientProtocolValidationTest
status=CRASH
rc=-1
seconds=4.985
stdoutLog=...ClientProtocolValidationTest.out.log
stderrLog=...ClientProtocolValidationTest.err.log
```

Both log files were empty when inspected after the run.

## Initial Diagnosis

This bucket is not explained by the four repeated `NoSuchMethodError`
signatures. The most likely possibilities are:

- native process termination before stderr flushing,
- an unhandled Windows process exception that did not reach CratonVM logging,
- runner-level child process observation after a very early process abort, or
- the same invalid Keycloak classpath causing an early VM exit before logging
  initialization.

The two classes use the new Keycloak JUnit 5 test framework, so they should be
rerun after the JUnit classpath mismatch is fixed. If they still exit with empty
logs, this bucket should be treated as a separate CratonVM process-failure bug.

## Repro

The original run used:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -ClassList C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\non238-tests-testsuite-20260702.tsv `
  -Category others -Vm craton -Jit on -Start 1 -Count 0 -Parallel 2 -TimeoutSec 600 `
  -RunName craton-others-20260702-01 `
  -KeycloakRoot C:\craton\CratonVM\apps\keycloak `
  -WorkDir C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite `
  -Exe C:\craton\CratonVM\target\release\cratonvm-keycloak-others-20260702-01.exe
```

## Evidence

```text
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\results.tsv
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.admin.client.ClientProtocolMapperTest.out.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.admin.client.ClientProtocolMapperTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.admin.client.ClientProtocolValidationTest.out.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.admin.client.ClientProtocolValidationTest.err.log
```

## Next Steps

- Rerun both classes individually with `-Parallel 1` after fixing the Keycloak
  JUnit classpath.
- Capture Windows process exit diagnostics and any crash dumps if `rc=-1`
  repeats.
- Add runner-side reporting for empty stdout/stderr abnormal exits so these are
  separated from linkage-error crashes in summary output.
