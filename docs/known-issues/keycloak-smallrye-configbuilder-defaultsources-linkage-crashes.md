# Keycloak SmallRyeConfigBuilder addDefaultSources linkage crashes

Status: open

Date observed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 3 `CRASH`
rows with this linkage warning:

```text
NoSuchMethodError
method="io/smallrye/config/SmallRyeConfigBuilder.addDefaultSources()Lio/smallrye/config/SmallRyeConfigBuilder;"
caller="org/keycloak/testframework/config/Config.initConfig()Lio/smallrye/config/SmallRyeConfig; @pc=10"
```

Affected classes:

```text
tests/base org.keycloak.tests.db.CaseSensitiveSchemaTest
tests/base org.keycloak.tests.db.PreserveSchemaCaseLiquibaseTest
tests/clustering org.keycloak.tests.clustering.JdbcPingCustomSchemaTest
```

## Representative Rows

```text
index=189
module=tests/base
class=org.keycloak.tests.db.CaseSensitiveSchemaTest
status=CRASH
rc=1
seconds=6.789
```

```text
index=375
module=tests/clustering
class=org.keycloak.tests.clustering.JdbcPingCustomSchemaTest
status=CRASH
rc=1
seconds=6.182
```

## Initial Diagnosis

The method exists in locally available SmallRye config jars:

```text
smallrye-config-core-3.2.1.jar
smallrye-config-core-3.15.1.jar
smallrye-config-core-3.16.0.jar
smallrye-config-core-3.17.2.jar
```

But `apps\keycloak\kc-universal-cp.txt` does not include
`smallrye-config-core`. A HotSpot probe of `JdbcPingCustomSchemaTest` with the
same runner/classpath fails as a normal JUnit failure due to missing test
framework service/provider classes, including:

```text
org.keycloak.testframework.authzen.client.AuthZenTestFrameworkExtension
org.infinispan.util.function.SerializableComparator
```

The current classification is classpath/setup bug, with CratonVM surfacing the
failure as a hard linkage exit before JUnit can emit a normal result summary.

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
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.db.CaseSensitiveSchemaTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_clustering.org.keycloak.tests.clustering.JdbcPingCustomSchemaTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\hotspot-crash-signature-probe-20260702-01\hotspot-jit\logs\tests_clustering.org.keycloak.tests.clustering.JdbcPingCustomSchemaTest.out.log
```

## Next Steps

- Rebuild the `tests/base` and `tests/clustering` runtime classpaths from Maven
  instead of relying on the current universal classpath.
- Confirm `smallrye-config-core`, Infinispan support, and Keycloak test
  framework service providers are present.
- Rerun the three affected classes under both HotSpot and CratonVM.
- If the HotSpot run becomes clean and CratonVM still reports
  `SmallRyeConfigBuilder.addDefaultSources` as missing, reduce
  `Config.initConfig()`.
