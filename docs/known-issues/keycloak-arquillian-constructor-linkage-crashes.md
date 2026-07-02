# Keycloak Arquillian constructor linkage crashes

Status: open

Date observed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 546
`CRASH` rows where CratonVM exits during test discovery with a linkage error for
the Arquillian JUnit runner constructor:

```text
NoSuchMethodError
method="org/jboss/arquillian/junit/Arquillian.<init>(Ljava/lang/Class;)V"
caller="org/keycloak/testsuite/arquillian/KcArquillian.<init>(Ljava/lang/Class;)V @pc=5"

[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
no such method: org/jboss/arquillian/junit/Arquillian.<init>(Ljava/lang/Class;)V
```

The failures are concentrated in the legacy Arquillian suite:

```text
testsuite/integration-arquillian/tests/base: 544
testsuite/integration-arquillian/tests/other/sssd: 2
```

## Representative Rows

```text
index=391
module=testsuite/integration-arquillian/tests/base
class=org.keycloak.testsuite.account.AccountRestServiceCorsTest
status=CRASH
rc=1
seconds=5.346
```

```text
index=395
module=testsuite/integration-arquillian/tests/base
class=org.keycloak.testsuite.account.AccountRestServiceTest
status=CRASH
rc=1
seconds=5.287
```

## Initial Diagnosis

This is not yet confirmed as 546 independent VM bugs. The expanded run used the
single generated `apps\keycloak\kc-universal-cp.txt` classpath. That classpath
is incomplete for the expanded Keycloak suite:

- It does not directly include `arquillian-junit-core`.
- A HotSpot probe of `AccountRestServiceTest` with the same runner/classpath
  fails during discovery because `org.keycloak.testsuite.util.oauth.AccessTokenResponse`
  is missing.
- `KcArquillian.class` is compiled to call
  `Arquillian.<init>(Ljava/lang/Class;)V`; `javap` shows that constructor exists
  in the locally available Arquillian JUnit jars.

The current suspicion is an invalid test runtime classpath and/or a CratonVM
method-resolution path that reports the missing superclass dependency as a
missing constructor. The first step is to rebuild a module-accurate classpath for
the Arquillian tests and rerun a small sample under both HotSpot and CratonVM.

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
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.account.AccountRestServiceCorsTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\hotspot-crash-signature-probe-20260702-01\hotspot-jit\results.tsv
```

## Next Steps

- Generate a module-specific Maven test classpath for
  `testsuite/integration-arquillian/tests/base`.
- Ensure `arquillian-junit-core` and Keycloak testsuite utility classes are on
  that classpath.
- Rerun a one-class HotSpot/CratonVM comparison.
- If HotSpot passes discovery but CratonVM still reports this constructor as
  missing, reduce to `KcArquillian.<init>` superclass invocation.
