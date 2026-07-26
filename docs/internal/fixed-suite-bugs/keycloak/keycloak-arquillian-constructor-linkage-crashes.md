# Keycloak Arquillian constructor linkage crashes

Status: fixed

Date observed: 2026-07-02
Date fixed: 2026-07-02

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

## Resolution

This was a Keycloak suite-runner classpath defect, not 546 independent
`Arquillian.<init>(Class)` VM linkage defects.

`apps\keycloak-suite-runner\run-keycloak-suite.ps1` now builds and caches a
Maven test runtime classpath per selected module. For long Windows classpaths it
generates a pathing JAR with a folded manifest `Class-Path`, so both HotSpot and
CratonVM can launch the same accurate dependency set without exceeding command
line limits.

For `testsuite/integration-arquillian/tests/base`, the generated classpath
contains:

- `keycloak-tests-utils-shared`, which provides
  `org.keycloak.testsuite.util.oauth.AccessTokenResponse`
- `arquillian-junit-core`
- matching JUnit Platform/Jupiter runtime entries inferred from the module's
  Maven dependency versions

## Original Diagnosis

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

The current suspicion was an invalid test runtime classpath and/or a CratonVM
method-resolution path that reports the missing superclass dependency as a
missing constructor.

The classpath suspicion was confirmed: once launched through the generated
module classpath and pathing JAR, HotSpot discovered 59 tests and CratonVM no
longer reported the Arquillian constructor as missing.

The same CratonVM probe then reached a later, separate linkage failure:

```text
NoSuchMethodError
method="java/lang/String.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
caller="java/lang/Package.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation; @pc=8"
```

That residual belongs to the existing package annotation reflection gap, not to
the Arquillian constructor issue.

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

## Verification

- HotSpot one-class probe with module pathing JAR:
  `org.keycloak.testsuite.account.AccountRestServiceTest`
  reached Arquillian discovery, found 59 tests, and failed later because
  `auth-server-undertow` was not configured.
- CratonVM one-class probe with the same pathing JAR no longer reported
  `org/jboss/arquillian/junit/Arquillian.<init>(Ljava/lang/Class;)V`.
