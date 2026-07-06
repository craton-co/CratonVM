# Keycloak test-framework EnterpriseDbDatabaseSupplier NoClassDefFoundError

Status: open

Date observed: 2026-07-06

## Summary

Discovered as the residual once
[keycloak-testframework-quarkus-config-classpath-gap](../internal/fixed-suite-bugs/keycloak-testframework-quarkus-config-classpath-gap.md)
was fixed (that fix got `Config.initConfig()` and Quarkus's
`LoggingSetupRecorder` bootstrapping cleanly; JUnit 5 now actually starts
running the test method). The very next thing that runs — `Registry`'s
supplier-extension discovery — fails:

```text
java.lang.NoClassDefFoundError: org/keycloak/testframework/database/EnterpriseDbDatabaseSupplier
   org.keycloak.testframework.database.EnterpriseDbTestFrameworkExtension.suppliers(EnterpriseDbTestFrameworkExtension.java:12)
   org.keycloak.testframework.injection.Extensions.loadSuppliers(Extensions.java:102)
   org.keycloak.testframework.injection.Extensions.<init>(Extensions.java:45)
   org.keycloak.testframework.injection.Extensions.getInstance(Extensions.java:31)
   org.keycloak.testframework.injection.Registry.<init>(Registry.java:44)
```

This is a normal JUnit-reported test failure (not a VM-crashing top-level
linkage error), so it does not block other test classes — but every class
that goes through `Registry`'s extension-supplier discovery presumably hits
it identically, since it happens unconditionally in `Registry`'s
constructor, not gated on the specific test's requested database.

## Puzzling part — this looks like a real CratonVM bug, not a classpath gap

Unlike the quarkus-core chain (where the missing class was genuinely absent
from any jar on the classpath), here:

- The **source** exists:
  `apps/keycloak/test-framework/db-edb/src/main/java/org/keycloak/testframework/database/EnterpriseDbDatabaseSupplier.java`.
- The **compiled class** exists:
  `apps/keycloak/test-framework/db-edb/target/classes/org/keycloak/testframework/database/EnterpriseDbDatabaseSupplier.class`.
- The **directory is on the classpath**:
  `kc-universal-cp.txt` contains
  `C:/craton/CratonVM/apps/keycloak/test-framework/db-edb/target/classes`.

Yet CratonVM reports `NoClassDefFoundError` for it anyway. `EnterpriseDbDatabaseSupplier extends AbstractContainerDatabaseSupplier` — the leading hypothesis (not yet confirmed) is that the superclass (or something in its static init / import chain, likely Testcontainers `org.testcontainers.*`, since this is a container-backed EnterpriseDB test database) fails to link, and Java's real `NoClassDefFoundError` semantics report the **subclass being loaded** rather than the deeper cause — i.e. this may be the same "misleading class name in the NoSuchMethodError/NoClassDefFoundError diagnostic" pattern documented in
[keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod](keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md),
or it may simply be a missing Testcontainers jar (a genuine classpath gap,
not a VM bug) — not distinguished yet.

## Repro

```powershell
$KC  = "C:/craton/CratonVM/apps/keycloak"
$CV  = "C:/craton/CratonVM/target/release/cratonvm.exe"
$JDK = "C:/Program Files/Java/jdk-25"
$CP  = "$KC/kc-runner;" + (Get-Content "$KC/kc-universal-cp.txt" -Raw).Trim()
& $CV --java-home $JDK --stack-dump-on-timeout 0 -cp $CP KcRunner org.keycloak.tests.account.AccountConsoleDisabledTest
```

## Next steps

- Check whether `AbstractContainerDatabaseSupplier` (and its supertype
  chain) needs `org.testcontainers:testcontainers` / `org.testcontainers:*`
  on the classpath — if so, this is the same class of classpath-completeness
  gap as the Quarkus config fix, just one module deeper
  (`test-framework/db-edb` → whatever declares the Testcontainers
  dependency).
- If Testcontainers is confirmed present and correctly resolvable via
  `javap`, this becomes a genuine CratonVM class-resolution bug (matching
  the isClustered() pattern) and should be investigated as such, ideally
  with a minimal standalone repro isolating just this one class hierarchy
  (not the full Keycloak/Quarkus/JUnit5 bootstrap).
