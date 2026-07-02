# Keycloak SmallRye Assert.assertNotNull linkage crashes

Status: open

Date observed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 37
`CRASH` rows in `testsuite/model` with this linkage warning:

```text
NoSuchMethodError
method="io/smallrye/common/constraint/Assert.assertNotNull(Ljava/lang/Object;)Ljava/lang/Object;"
caller="org/keycloak/config/OptionBuilder.expectedValues(Ljava/util/List;)Lorg/keycloak/config/OptionBuilder; @pc=4"
```

The final process error then reports:

```text
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
no class def found: org/keycloak/testsuite/model/KeycloakModelTest
```

## Representative Rows

```text
index=1013
module=testsuite/model
class=org.keycloak.testsuite.model.authz.ConcurrentAuthzTest
status=CRASH
rc=1
seconds=14.519
```

```text
index=1014
module=testsuite/model
class=org.keycloak.testsuite.model.client.ClientModelTest
status=CRASH
rc=1
seconds=11.850
```

## Initial Diagnosis

This appears to be another expanded-suite classpath problem first, with a
possible CratonVM linkage-reporting issue second.

`javap` shows `Assert.assertNotNull(Object)Object` exists in the locally
available SmallRye common constraint jars:

```text
smallrye-common-constraint-2.1.0.jar
smallrye-common-constraint-2.16.0.jar
smallrye-common-constraint-2.17.1.jar
```

However, `apps\keycloak\kc-universal-cp.txt` does not include
`smallrye-common-constraint`. A HotSpot probe of representative model test
`ClientModelTest` does not reproduce this exact CratonVM `NoSuchMethodError`;
it fails as a normal JUnit failure because `io.micrometer.core.instrument.MeterRegistry`
is missing from the same runtime classpath.

The current suspicion is an incomplete model-test runtime classpath. The
CratonVM message should still be investigated after fixing the classpath,
because the stderr shows a missing SmallRye method even though the final error
mentions missing `KeycloakModelTest`.

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
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\testsuite_model.org.keycloak.testsuite.model.client.ClientModelTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\hotspot-crash-signature-probe-20260702-01\hotspot-jit\logs\testsuite_model.org.keycloak.testsuite.model.client.ClientModelTest.out.log
```

## Next Steps

- Generate a correct Maven test runtime classpath for `testsuite/model`.
- Confirm `smallrye-common-constraint`, `micrometer-core`, and model test
  support classes are present.
- Rerun one representative model class under HotSpot and CratonVM.
- If HotSpot passes discovery and CratonVM still reports
  `Assert.assertNotNull` as missing, reduce the `OptionBuilder.expectedValues`
  call path.
