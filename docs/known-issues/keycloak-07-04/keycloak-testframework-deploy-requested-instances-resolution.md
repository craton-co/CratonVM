# Keycloak test-framework deployRequestedInstances resolution failure

Status: open - surfaced after fixing the SmallRye Charset/MemorySize converter blocker

Date observed: 2026-07-04

## Summary

After `LoggingSetupRecorder.handleFailedStart()` keeps the Quarkus converters and the `SRCFG00013` `Charset` / `MemorySize` validation failure is gone, `tests/base :: org.keycloak.tests.admin.identityprovider.IdentityProviderMapperTest` reaches its test methods. All 5 methods then fail in `beforeEach` with:

```
java.lang.RuntimeException: Failed to resolve next requested instance to deploy
    org.keycloak.testframework.injection.Registry.lambda$deployRequestedInstances$1(Registry.java:248)
    org.keycloak.testframework.injection.Registry.deployRequestedInstances(Registry.java:248)
    org.keycloak.testframework.injection.Registry.beforeEach(Registry.java:132)
    org.keycloak.testframework.KeycloakIntegrationTestExtension.beforeEach(KeycloakIntegrationTestExtension.java:30)
```

`KCRUNNER_RESULT tests=5 failed=5 aborted=0 skipped=0 containersFailed=0`. This is a later test-framework dependency-resolution issue, not the previous container-level logging config failure.

## Evidence

- Fixed run: `apps/keycloak-suite-runner/.suite/results/kc0704-smallrye-identityprovider-fixed-172634/all-jit/`
- Before fix: same class failed before any test method ran with `SRCFG00013: No Converter registered for class java.nio.charset.Charset` and `io.quarkus.runtime.configuration.MemorySize`.
