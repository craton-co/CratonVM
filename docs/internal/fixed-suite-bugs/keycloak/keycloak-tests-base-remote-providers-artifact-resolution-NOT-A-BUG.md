# Keycloak `tests/base` — `keycloak-test-framework-remote-providers` artifact resolution failure

## Status: ✅ NOT A CRATONVM BUG — confirmed environment/Maven-setup gap (triaged 2026-07-07)

**Reproduces identically under real HotSpot** (same class, same harness, `--java-home`
swapped for a plain JDK 25 `java`): `org.keycloak.tests.model.SimpleModelTest` fails
with the exact same `Failed to resolve artifact:
org.keycloak.testframework:keycloak-test-framework-remote-providers` error and stack
trace under both VMs. Since this is a pure Maven-reactor dependency-resolution failure
inside Keycloak's own `org.keycloak.it.utils.Maven` helper — nothing CratonVM-specific
is involved — this is an environment/checkout prerequisite gap (likely needs a full
reactor `mvn install` of the whole `keycloak-parent` tree, network access to check for
snapshot updates, or a `settings.xml` this checkout lacks), not a CratonVM defect. Moved
out of `known-issues` per this project's triage convention; kept for context in case the
underlying environment gap is worth fixing to unblock further `tests/base` measurement.

## Context

Found while verifying the fix for
[the X.509 `AuthorityKeyIdentifier` NPE](../keycloak-x509-authoritykeyidentifier-npe-FIXED.md),
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

**Resolved**: confirmed via a direct HotSpot repro (`-Vm hotspot`, same
`SimpleModelTest`, same harness) that this fails identically under real
HotSpot — same exception, same stack trace, same message. This rules out a
CratonVM-side native/real-bytecode gap; the failure is entirely inside
Keycloak's own `org.keycloak.it.utils.Maven` reactor-resolution helper and
would need to be fixed at the environment/checkout level (full reactor `mvn
install`, network access, or a missing `settings.xml`) regardless of which
JVM runs the tests.

## Impact

Blocks a real pass/fail measurement of the `tests/base` module even after the
X.509 fix above — every class that provisions a `DistributionKeycloakServer`
(likely the large majority) hits this during `@BeforeEach`/`Registry.beforeEach`
provider deployment, before its actual test body runs.
