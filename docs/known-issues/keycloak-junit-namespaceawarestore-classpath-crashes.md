# Keycloak JUnit NamespaceAwareStore classpath crashes

Status: open

Date observed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 338
`CRASH` rows where JUnit 5 extension setup fails with:

```text
NoSuchMethodError
method="org/junit/jupiter/engine/execution/NamespaceAwareStore.computeIfAbsent(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;"
caller="org/keycloak/testframework/KeycloakIntegrationTestExtension.getLogHandler(Lorg/junit/jupiter/api/extension/ExtensionContext;)Lorg/keycloak/testframework/LogHandler; @pc=28"
```

The failures are mostly in the new Keycloak JUnit 5 test framework:

```text
tests/base: 337
tests/clustering: 1
```

## Representative Rows

```text
module=tests/base
class=org.keycloak.tests.account.AccountConsoleDisabledTest
status=CRASH
rc=1
seconds=14.581
```

```text
module=tests/base
class=org.keycloak.tests.actions.RequiredActionUpdateProfileTest
status=CRASH
rc=1
seconds=14.360
```

## Initial Diagnosis

This signature is strongly tied to a mixed JUnit runtime classpath, not to 338
separate test defects.

`apps\keycloak\kc-universal-cp.txt` contains both old and new JUnit artifacts,
with the older jars appearing first:

```text
org/junit/jupiter/junit-jupiter-api/5.10.3
org/junit/jupiter/junit-jupiter-api/6.0.3
org/junit/jupiter/junit-jupiter-engine/5.10.3
org/junit/jupiter/junit-jupiter-engine/6.0.3
org/junit/platform/junit-platform-commons/1.10.3
org/junit/platform/junit-platform-commons/6.0.3
org/junit/platform/junit-platform-engine/1.10.3
org/junit/platform/junit-platform-engine/6.0.3
```

`javap` confirms the mismatch:

- `junit-jupiter-engine-5.10.3.jar` does not have
  `NamespaceAwareStore.computeIfAbsent(...)`.
- `junit-jupiter-engine-6.0.3.jar` does have
  `NamespaceAwareStore.computeIfAbsent(...)`.

A HotSpot probe with the same runner/classpath also fails this representative
class with a JUnit `NoSuchMethodError`, so this case is currently classified as
a runner/classpath bug. CratonVM reports it as process `CRASH` because the
linkage error terminates the VM before the JUnit summary line is emitted.

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

HotSpot probe result:

```text
run=hotspot-crash-signature-probe-20260702-01
class=org.keycloak.tests.account.AccountConsoleDisabledTest
status=FAIL
note=java.lang.NoSuchMethodError: ExtensionContext$Store.computeIfAbsent(...)
```

## Evidence

```text
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\results.tsv
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.account.AccountConsoleDisabledTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\hotspot-crash-signature-probe-20260702-01\hotspot-jit\logs\tests_base.org.keycloak.tests.account.AccountConsoleDisabledTest.out.log
```

## Next Steps

- Rebuild `kc-universal-cp.txt` or replace it with module-specific test
  classpaths so only one compatible JUnit stack is present.
- Prefer JUnit 6 artifacts for the new Keycloak test framework classes that call
  `computeIfAbsent`.
- Rerun the 338 affected classes after classpath normalization.
- Adjust runner status classification if CratonVM linkage exits should be
  reported separately from native crashes.
