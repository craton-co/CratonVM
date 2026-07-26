# Keycloak universal classpath residuals after EnterpriseDB supplier fix

Status: fixed

Date observed: 2026-07-08
Date fixed: 2026-07-08

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

The follow-up fix teaches
`../../../../apps/keycloak-suite-runner/generate-kc-universal-cp.ps1` to include:

- a filtered `test-framework/ui` runtime closure for Selenium, HtmlUnit, and
  direct Selenium driver support libraries;
- the exact Quarkus `quarkus-bootstrap-maven-resolver` artifact needed by
  `BootstrapMavenContext`;
- the Maven Resolver, Maven model, Sisu, Plexus, and BeanBag support groups
  linked by that resolver artifact.

Validation also exposed a runner-only pathing-jar ordering bug: with long
universal classpaths, `run-keycloak-suite.ps1` built a pathing jar whose
manifest put `kc-runner` ahead of the selected module output dirs. Keycloak's
`Maven.bootstrapCurrentMavenContext()` then derived the current project from
`kc-runner`/the reactor parent instead of `tests/base`, so
`Maven.resolveArtifact()` could not find
`keycloak-test-framework-remote-providers`. The runner now orders the selected
module's `target/classes` and `target/test-classes` before `kc-runner` for both
module and universal classpaths.

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

## Fixed Validation

Using a scratch generated universal classpath from the fixed generator and a
one-class runner list:

```powershell
$KC = "C:/craton/CratonVM/apps/keycloak"
$WT = "C:/craton/CratonVM-keycloak-post-enterprisedb-residuals-20260708-001"
$Scratch = "$WT/.scratch-hhsf"
powershell -NoProfile -File "$WT/apps/keycloak-suite-runner/generate-kc-universal-cp.ps1" `
  -KeycloakRoot $KC `
  -OutFile "$Scratch/kc-fixed-cp.txt" `
  -WorkDir "$Scratch/suite" `
  -Apply
```

Focused HotSpot class/link probe:

```text
SOURCE org.openqa.selenium.WebDriver file:/C:/Users/Victor/.m2/repository/org/seleniumhq/selenium/selenium-api/4.39.0/selenium-api-4.39.0.jar
SUPPLIERS 6
SOURCE io.quarkus.bootstrap.resolver.maven.BootstrapMavenContext file:/C:/Users/Victor/.m2/repository/io/quarkus/quarkus-bootstrap-maven-resolver/3.33.1.1/quarkus-bootstrap-maven-resolver-3.33.1.1.jar
CONFIG io.quarkus.bootstrap.resolver.maven.BootstrapMavenContextConfig
```

Runner validation with the fixed generator, fixed module-first pathing-jar
ordering, and unique CratonVM binary
`target/release/cratonvm-keycloak-post-enterprisedb-residuals-20260708-001.exe`:

```text
HotSpot:
  run: keycloak-post-enterprisedb-hotspot-20260708-002
  class: tests/base :: org.keycloak.tests.account.AccountConsoleDisabledTest
  status: FAIL
  residual: java.lang.IllegalStateException: Could not find a valid Docker environment

CratonVM:
  run: keycloak-post-enterprisedb-craton-20260708-001
  class: tests/base :: org.keycloak.tests.account.AccountConsoleDisabledTest
  status: FAIL
  residual: java.lang.IllegalStateException: Could not find a valid Docker environment
```

The validation no longer contains the fixed signatures:

- `NoClassDefFoundError: org/openqa/selenium/WebDriver`
- `NoSuchMethodError: BootstrapMavenContext.config()`
- `Failed to resolve artifact: keycloak-test-framework-remote-providers`

Both HotSpot and CratonVM now reach the same local environmental boundary:
Testcontainers cannot find a valid Docker environment on this host.

## Follow-up Recheck: origin/dev Reconciliation

On 2026-07-08, `origin/dev` still carried the old open
`docs/known-issues/keycloak-universal-classpath-post-enterprisedb-residuals.md`
record while local `dev` already had the fix and this archived note. Merging
`origin/dev` into
`codex/fix-keycloak-post-enterprisedb-followup-20260708-002` kept the fixed
state: the known-issues path remains absent, this archive remains present, and
the generator/runner fixes listed above are still in the merged tree.
