# Keycloak universal classpath residuals after EnterpriseDB supplier fix

Status: open

Date observed: 2026-07-08

## Summary

After the EnterpriseDB supplier classpath fix, direct class-load probes pass on
both HotSpot and CratonVM for the classes that had blocked the previous
residual:

```text
LOAD_OK org.keycloak.testframework.database.EnterpriseDbDatabaseSupplier
LOAD_OK org.keycloak.testframework.ServerConfigClassOrderer
LOAD_OK io.quarkus.maven.dependency.DependencyBuilder
LOAD_OK io.quarkus.bootstrap.utils.BuildToolHelper
LOAD_OK
```

The original `AccountConsoleDisabledTest` repro now gets past
`Registry` supplier discovery far enough to expose later, unrelated universal
classpath gaps.

## HotSpot residual: Selenium WebDriver

HotSpot now fails while loading the UI test-framework provider:

```text
java.lang.NoClassDefFoundError: org/openqa/selenium/WebDriver
   org.keycloak.testframework.ui.UITestFrameworkExtension.suppliers(UITestFrameworkExtension.java:21)
   org.keycloak.testframework.injection.Extensions.loadSuppliers(Extensions.java:102)
   org.keycloak.testframework.injection.Extensions.<init>(Extensions.java:45)
   org.keycloak.testframework.injection.Extensions.getInstance(Extensions.java:31)
   org.keycloak.testframework.injection.Registry.<init>(Registry.java:44)
Caused by: java.lang.ClassNotFoundException: org.openqa.selenium.WebDriver
```

This is the same broad pattern as the fixed EnterpriseDB supplier: the
universal classpath exposes `test-framework/ui/target/classes` and its
`TestFrameworkExtension` service provider, but does not yet include the Selenium
runtime closure needed to link the provider.

## CratonVM residual: BootstrapMavenContext.config()

CratonVM now reaches Keycloak server startup and fails later:

```text
java.lang.NoSuchMethodError:
  io/quarkus/bootstrap/resolver/maven/BootstrapMavenContext.config()
  Lio/quarkus/bootstrap/resolver/maven/BootstrapMavenContextConfig;
   org.keycloak.it.utils.Maven.bootstrapCurrentMavenContext(Maven.java:159)
   org.keycloak.it.utils.Maven.getKeycloakQuarkusModulePath(Maven.java:142)
   org.keycloak.it.utils.DockerKeycloakDistribution.createImage(DockerKeycloakDistribution.java:113)
   org.keycloak.testframework.server.ClusteredKeycloakServer.defaultImage(ClusteredKeycloakServer.java:47)
```

The 2026-07-08 generator refresh already adds `quarkus-bootstrap-core`, which
contains `BuildToolHelper`, but it does not add the Maven resolver artifact
closure behind `BootstrapMavenContext`. This may be another classpath gap
(`quarkus-bootstrap-maven-resolver` and Maven resolver dependencies), but it
should be checked against HotSpot after the Selenium residual is cleared.

## Repro

Use a generated universal classpath from the fixed generator, then run:

```powershell
$KC  = "C:/craton/CratonVM/apps/keycloak"
$CV  = "C:/craton/CratonVM-keycloak-enterprisedb-supplier-20260708-001/target/release/cratonvm-keycloak-enterprisedb-supplier-20260708-001.exe"
$JDK = "C:/Program Files/Java/jdk-25"
$CP  = "$KC/kc-runner;" + (Get-Content "C:/path/to/generated/kc-fixed-cp.txt" -Raw).Trim()
& $CV --java-home $JDK --stack-dump-on-timeout 0 --Xmx 2g `
  -Dfile.encoding=UTF-8 -Djava.awt.headless=true `
  -cp $CP KcRunner org.keycloak.tests.account.AccountConsoleDisabledTest
```

For HotSpot, replace the executable with:

```powershell
& "$JDK/bin/java.exe" -Xmx2g -Dfile.encoding=UTF-8 -Djava.awt.headless=true `
  -cp $CP KcRunner org.keycloak.tests.account.AccountConsoleDisabledTest
```

## Next steps

- Add a filtered default classpath closure for `test-framework/ui` if the
  universal classpath is expected to expose its `TestFrameworkExtension`
  provider unconditionally.
- After HotSpot reaches the same server-start path, determine whether
  `BootstrapMavenContext.config()` is another missing Quarkus Maven resolver
  artifact or a CratonVM-specific method-resolution problem.
