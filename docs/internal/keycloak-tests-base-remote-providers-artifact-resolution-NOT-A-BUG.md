# Keycloak `tests/base` — `keycloak-test-framework-remote-providers` artifact resolution failure

## Status: 🔴 OPEN, untriaged (newly surfaced 2026-07-07)

## Context

Found while verifying the fix for
[the X.509 `AuthorityKeyIdentifier` NPE](../internal/fixed-suite-bugs/keycloak-x509-authoritykeyidentifier-npe-FIXED.md),
which used to block essentially all of `tests/base` before even reaching this
point. With that fix in place, `tests/base` classes now boot a real embedded
Keycloak distribution server (crypto provider init, HTTPS keystore/truststore
generation via `ManagedCertificates` all succeed) but then fail during
per-test server startup/provider deployment.

## Symptom

Every `tests/base` class tried so far (`org.keycloak.tests.model.SimpleModelTest`,
`org.keycloak.tests.admin.AdminRootTest`) fails every `@Test` method with:

```
java.lang.RuntimeException: Failed to resolve artifact: org.keycloak.testframework:keycloak-test-framework-remote-providers
    org.keycloak.it.utils.Maven.getArtifact(Maven.java:88)
    org.keycloak.it.utils.Maven.resolveArtifact(Maven.java:51)
    org.keycloak.testframework.server.ProviderDeployer.getDependencyPath(ProviderDeployer.java:119)
    org.keycloak.testframework.server.ProviderDeployer.updateDependencies(ProviderDeployer.java:43)
    org.keycloak.testframework.server.DistributionKeycloakServer.start(DistributionKeycloakServer.java:93)
    org.keycloak.testframework.server.AbstractKeycloakServerSupplier.getValue(AbstractKeycloakServerSupplier.java:82)
    org.keycloak.testframework.injection.Registry.deployRequestedInstances(Registry.java:254)
    org.keycloak.testframework.injection.Registry.beforeEach(Registry.java:132)
Caused by: java.lang.RuntimeException: Failed to resolve artifact [org.keycloak.testframework:keycloak-test-framework-remote-providers] from project [org.keycloak:keycloak-parent:pom:999.0.0-SNAPSHOT] dependency graph
    org.keycloak.it.utils.Maven.getArtifact(Maven.java:79)
```

## What's already ruled out

The jar/pom for `keycloak-test-framework-remote-providers` (version
`999.0.0-SNAPSHOT`) **is present** in the local Maven repository
(`~/.m2/repository/org/keycloak/testframework/keycloak-test-framework-remote-providers/`),
so this is not a simple "module was never built" gap. `org.keycloak.it.utils.Maven`
appears to do its own in-process dependency-graph resolution (reading the
`keycloak-parent` reactor POM) rather than a flat local-repo lookup — that
resolution is what's failing, not the artifact's physical presence.

Not yet determined whether this is:
- a genuine CratonVM bug (some native/real-bytecode gap in whatever
  Maven-model-resolution machinery `org.keycloak.it.utils.Maven` depends on
  — e.g. an incomplete `javax.xml`/DOM parse, a `ServiceLoader` gap in the
  Maven resolver library, or similar), or
- an environment/setup gap specific to this local checkout (e.g. needs a full
  reactor `mvn install`, network access to check for updates, or a
  `settings.xml` this session doesn't have) that would also fail under real
  HotSpot and isn't a CratonVM defect at all.

**Next step**: reproduce the identical `KcRunner` invocation under real
HotSpot (`--java-home` swapped for a plain `java` run, same classpath) to
determine whether this is CratonVM-specific or an environment prerequisite
that both VMs would hit equally. If it reproduces under HotSpot too, this
doc should be reclassified as an environment-setup task, not a CratonVM bug.

## Impact

Blocks a real pass/fail measurement of the `tests/base` module even after the
X.509 fix above — every class that provisions a `DistributionKeycloakServer`
(likely the large majority) hits this during `@BeforeEach`/`Registry.beforeEach`
provider deployment, before its actual test body runs.
