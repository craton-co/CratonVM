# Keycloak test-framework EnterpriseDbDatabaseSupplier NoClassDefFoundError

Status: fixed on 2026-07-08

Date observed: 2026-07-06

## Summary

This was the residual exposed after
[keycloak-testframework-quarkus-config-classpath-gap](keycloak-testframework-quarkus-config-classpath-gap.md)
got `Config.initConfig()` and Quarkus logging bootstrap running far enough for
JUnit 5 to start the test method. `Registry` extension-supplier discovery then
failed while loading the EnterpriseDB test-framework provider:

```text
java.lang.NoClassDefFoundError: org/keycloak/testframework/database/EnterpriseDbDatabaseSupplier
   org.keycloak.testframework.database.EnterpriseDbTestFrameworkExtension.suppliers(EnterpriseDbTestFrameworkExtension.java:12)
   org.keycloak.testframework.injection.Extensions.loadSuppliers(Extensions.java:102)
   org.keycloak.testframework.injection.Extensions.<init>(Extensions.java:45)
   org.keycloak.testframework.injection.Extensions.getInstance(Extensions.java:31)
   org.keycloak.testframework.injection.Registry.<init>(Registry.java:44)
```

The root cause was a classpath-completeness gap, not a CratonVM class
resolution bug. The named class was present in
`test-framework/db-edb/target/classes`, but its superclass
`org.keycloak.testframework.database.AbstractContainerDatabaseSupplier` was not:
`test-framework/test-containers/target/classes` was missing from the universal
classpath, and the Testcontainers/Docker runtime jars needed by that module
were missing too.

HotSpot made the missing superclass explicit with a direct `Class.forName`:

```text
java.lang.NoClassDefFoundError: org/keycloak/testframework/database/AbstractContainerDatabaseSupplier
Caused by: java.lang.ClassNotFoundException:
  org.keycloak.testframework.database.AbstractContainerDatabaseSupplier
```

## Fix

`../../../../apps/keycloak-suite-runner/generate-kc-universal-cp.ps1` now covers this
provider classpath by default without requiring a hand-maintained jar list:

- Builds a filtered Maven runtime classpath for `test-framework/db-edb` when no
  precomputed `cratonvm-full-cp.txt` exists.
- Adds the specific Testcontainers/Docker support groups needed by the EDB
  provider: `org.testcontainers`, `com.github.docker-java`, `org.rnorth`,
  `org.apache.commons`, and `org.jetbrains`.
- Still adds all usable Keycloak module output directories, including
  `test-framework/test-containers/target/classes`.
- Prunes stale `target/classes` / `target/test-classes` entries that contain
  `../../../../apps/META-INF/services` descriptors but no compiled `.class` files; those
  resource-only stale service descriptors can otherwise break every test via
  `ServiceLoader`.

The same generator refresh also covers two adjacent universal-classpath gaps
that surfaced immediately after the EDB provider loaded:

- `test-framework/junit5-config` now contributes the Infinispan runtime jars
  needed by `ServerConfigClassOrderer`.
- `test-framework/core` now contributes exact Quarkus bootstrap artifacts
  `quarkus-bootstrap-app-model` and `quarkus-bootstrap-core`.

## Validation

Validation used a throwaway classpath copy, not the external checkout's
untracked `../../../../apps/keycloak/kc-universal-cp.txt`:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass `
  -File apps\keycloak-suite-runner\generate-kc-universal-cp.ps1 `
  -KeycloakRoot C:\craton\CratonVM\apps\keycloak `
  -OutFile C:\craton\CratonVM-keycloak-enterprisedb-supplier-20260708-001\.scratch-hhsf\kc-fixed-cp.txt `
  -WorkDir C:\craton\CratonVM-keycloak-enterprisedb-supplier-20260708-001\.scratch-hhsf\suite `
  -Apply
```

That generated classpath removed the stale AuthZen service-provider output dir,
added `test-framework/test-containers/target/classes`, Testcontainers/Docker
jars, Infinispan jars, and the two Quarkus bootstrap jars.

Focused load probes then passed on both HotSpot and CratonVM:

```text
LOAD_OK org.keycloak.testframework.database.EnterpriseDbDatabaseSupplier
LOAD_OK org.keycloak.testframework.ServerConfigClassOrderer
LOAD_OK io.quarkus.maven.dependency.DependencyBuilder
LOAD_OK io.quarkus.bootstrap.utils.BuildToolHelper
LOAD_OK
```

The CratonVM run used a unique binary copy:

```text
target/release/cratonvm-keycloak-enterprisedb-supplier-20260708-001.exe
```

The full `AccountConsoleDisabledTest` repro no longer reports
`EnterpriseDbDatabaseSupplier`, `AbstractContainerDatabaseSupplier`,
`SerializableComparator`, `DependencyBuilder`, or `BuildToolHelper` as missing.
It now reaches later, separate residuals documented in
[keycloak-universal-classpath-post-enterprisedb-residuals.md](keycloak-universal-classpath-post-enterprisedb-residuals.md).
