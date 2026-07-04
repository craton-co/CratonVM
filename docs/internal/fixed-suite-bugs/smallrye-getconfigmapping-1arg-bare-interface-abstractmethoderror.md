# SmallRyeConfig.getConfigMapping(Class) 1-arg form allocated a bare interface — AbstractMethodError

Status: FIXED (2026-07-04)

## Summary

5 `quarkus/deployment` classes crashed identically during JUnit test
discovery (never ran a single `@Test`):

```
Caused by: java.lang.AbstractMethodError: method
  io/quarkus/deployment/dev/testing/TestConfig.classOrderer()Ljava/util/Optional;
  has no Code attribute
    at org/junit/jupiter/engine/discovery/ClassOrderingVisitor.<init>(ClassOrderingVisitor.java:49)
    ...
    at io/quarkus/test/config/QuarkusClassOrderer.<init>(QuarkusClassOrderer.java:29)
```

Affected: `PersistenceXmlDatasourcesTest`,
`test.org.keycloak.quarkus.services.health.KeycloakMetricsConfigurationTest`,
`KeycloakNegativeHealthCheckTest`, `KeycloakPathConfigurationTest`,
`KeycloakReadyHealthCheckTest`.

## Root cause

This is the exact same bug class as the already-fixed Keycloak-Quarkus-boot
"Gap 6" (`docs/internal/app-jvm-bugs/keycloak-quarkus-boot-progress.md`):
`SmallRyeConfig.getConfigMapping(Class, String)` (the 2-arg form) was fixed
to build the real, config-backed `<iface>$$CMImpl` via
`ConfigMappingLoader.configMappingObject(...)` instead of allocating a bare
instance of the mapping *interface itself*. But the **1-arg overload**
(`getConfigMapping(Class)`, used here to obtain `TestConfig` — real
SmallRye's 1-arg form is `getConfigMapping(type, getPrefixFromConfigMapping(type))`,
delegating to the 2-arg form with a prefix read off the `@ConfigMapping`
annotation) still had its own, separate, never-updated registration in
`native-builtins/src/phases_late.rs`:

```rust
r.register(
    "io/smallrye/config/SmallRyeConfig",
    "getConfigMapping",
    "(Ljava/lang/Class;)Ljava/lang/Object;",
    |ctx, args| {
        // ...
        let obj = alloc_concurrent_synthetic(ctx, &cls_name, 0);
        Ok(Some(Value::Object(Some(obj))))
    },
);
```

`alloc_concurrent_synthetic(ctx, &cls_name, 0)` allocates an instance of the
mapping *interface itself* (0 fields, no method bodies) — so the first
interface method call on it (`TestConfig.classOrderer()`) throws
`AbstractMethodError`, exactly the pre-Gap-6-fix failure mode, just reached
through the 1-arg overload the Gap-6 fix never touched.

## Fix

`native-builtins/src/phases_late.rs`:
- Added `config_mapping_prefix(ctx, cls)`: reads the real
  `@ConfigMapping(prefix = ...)` annotation off the mapping interface via
  `cls.getAnnotation(ConfigMapping.class).prefix()` (real reflection calls,
  same as what real `getConfigMapping(Class)` bytecode does internally),
  defaulting to Java `null` if the interface has no such annotation.
- The 1-arg registration now derives that prefix and delegates directly to
  `native_smallrye_get_config_mapping` (the already-correct 2-arg
  implementation) instead of the old bare-interface allocation. No behavior
  change for the 2-arg form; the 1-arg form now takes the exact same
  real-`$$CMImpl`-construction path.

## Verified

- Before: `PersistenceXmlDatasourcesTest` → CRASH, `AbstractMethodError:
  TestConfig.classOrderer()`.
- After: same class → the `AbstractMethodError` is gone; execution advances
  further and (after a second, independent classpath fix — see below) the
  class now runs to completion, reaching a *different*, later, unrelated
  failure (`SRCFG00013` — see
  `docs/known-issues/smallrye-config-missing-charset-memorysize-converters.md`).
  This confirms the fix is genuine and doesn't merely move the failure
  sideways within the same bug.
- A second, harness-side fix was needed to get this far: `run-keycloak-suite.ps1`'s
  `Add-JUnitPlatformInfraEntries` added `junit-vintage-engine` to every
  module's classpath (to bridge JUnit4-only modules through the JUnit
  Platform Launcher) but not vintage-engine's own dependency on the real
  `junit:junit` runtime jar (`org.junit.runner.Version` et al.) — without it,
  `quarkus/deployment` classes hit `class file error: class not found:
  junit/runner/Version` immediately after this fix. Added `junit:junit` to
  the same infra-jar list. This is a test-harness classpath gap, not a
  CratonVM bug (25 CRASH classes across `scim/core`, `ssf/core`,
  `ssf/transmitter`, `test-framework/*`, `tests/webauthn`,
  `tests/clustering` were affected by the missing-`junit:junit` gap alone).

## Evidence

Repro / verification runs: `/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/{verify-cmfix,verify-cmfix2}/all-jit/logs/` on the Azure build host, branch `test/keycloak-fullsuite-20260704`.
