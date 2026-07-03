# Keycloak clustering Quarkus TestConfig config-mapping implementation missing getSecrets

Status: open

Date observed: 2026-07-03

## Summary

After updating from `dev` and rerunning only the previously non-passed
Keycloak classes with the per-module classpath runner, both `tests/clustering`
classes still crash under CratonVM during JUnit launcher session startup.

The active signature is a method-resolution failure on Quarkus' generated
SmallRye config-mapping implementation:

```text
NoSuchMethodError
method="io/quarkus/deployment/dev/testing/TestConfig$$CMImpl.getSecrets()Ljava/util/Set;
[class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="io/smallrye/config/ConfigMappingLoader.configMappingSecrets(Ljava/lang/Class;)Ljava/util/Set; @pc=16"
```

The Java-visible exception is:

```text
java/lang/reflect/UndeclaredThrowableException
Caused by: java/lang/NoSuchMethodError:
io/quarkus/deployment/dev/testing/TestConfig$$CMImpl.getSecrets()Ljava/util/Set;
```

## Evidence

Run:

```text
craton-nonpassed-dev-20260703-01 / others-jit
```

Result file:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\results.tsv
```

Representative stderr log:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\tests_clustering.org.keycloak.tests.clustering.JdbcPingCustomSchemaTest.err.log
```

Affected classes:

```text
org.keycloak.tests.clustering.JdbcPingCustomSchemaTest
org.keycloak.tests.compatibility.ClusteredOAuthClientTest
```

The stack enters Quarkus test configuration before the crash:

```text
io.quarkus.test.config.ConfigLauncherSession.launcherSessionOpened
io.quarkus.test.config.TestConfigProviderResolver.getConfig
io.smallrye.config.SmallRyeConfigBuilder.build
io.quarkus.deployment.dev.testing.TestConfigCustomizer.configBuilder
io.smallrye.config.ConfigMappings$ConfigClass.configClass
io.smallrye.config.ConfigMappingLoader.configMappingSecrets
```

## Repro

Use the module classpath runner against one affected class:

```powershell
$list = "C:\temp\keycloak-clustering-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"tests/clustering`torg.keycloak.tests.clustering.JdbcPingCustomSchemaTest" | Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list `
  -Category others `
  -Vm craton `
  -Jit on `
  -Parallel 1 `
  -TimeoutSec 600 `
  -RunName keycloak-clustering-testconfig-cmimpl-repro `
  -KeycloakRoot "C:\craton\CratonVM\apps\keycloak" `
  -WorkDir "C:\craton\CratonVM\apps\keycloak-suite-runner\.suite" `
  -Exe "C:\craton\CratonVM\target\release\cratonvm.exe"
```

## Current assessment

CratonVM's diagnostic reports that
`io/quarkus/deployment/dev/testing/TestConfig$$CMImpl` was not found on any
classpath entry and was replaced by a synthetic stub. SmallRye then calls
`getSecrets()` on that generated mapping class and CratonVM reports a
`NoSuchMethodError` against the stub.

This may be a runner/module-classpath gap for Quarkus-generated config mapping
classes, or a CratonVM classloading/resource-generation difference where the
generated implementation should be discoverable but is not. A HotSpot
comparison using the same generated `tests/clustering` classpath is still
required before classifying the bug as VM-core versus suite classpath.

## Next steps

- Inspect the generated `tests/clustering` classpath and local Maven artifacts
  for a real `io/quarkus/deployment/dev/testing/TestConfig$$CMImpl` class.
- Re-run one affected class under HotSpot with the exact same classpath to
  determine whether the class is genuinely absent or only absent to CratonVM.
- If HotSpot finds the class, trace CratonVM class/resource lookup for
  `TestConfig$$CMImpl` and the SmallRye config-mapping generated-class path.
