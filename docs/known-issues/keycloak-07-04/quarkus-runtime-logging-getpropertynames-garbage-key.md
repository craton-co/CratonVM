# SmallRyeConfig.getPropertyNames() surfaces a garbage property name during wildcard log-category validation

Status: open

Date observed: 2026-07-04

## Summary

Two sub-tests in `quarkus/runtime :: LoggingConfigurationTest`
(`jsonEcsFormat`, `fileRotationCustomValues`) fail identically — not on an
assertion, but on a real `PropertyException` thrown during test setup
(`AbstractConfigurationTest.createConfig`, called from `initConfig`, before
the test body even runs its own logic):

```
org.keycloak.quarkus.runtime.cli.PropertyException: logging category 'reproducer.not^ok' is not valid
    org.keycloak.quarkus.runtime.configuration.mappers.LoggingPropertyMappers.validateLogLevel(LoggingPropertyMappers.java:423)
    org.keycloak.quarkus.runtime.configuration.mappers.LoggingPropertyMappers.lambda$parseRootLogLevel$0(LoggingPropertyMappers.java:465)
    org.keycloak.quarkus.runtime.configuration.mappers.LoggingPropertyMappers.parseRootLogLevel(LoggingPropertyMappers.java:463)
    org.keycloak.quarkus.runtime.configuration.mappers.LoggingPropertyMappers.getConfiguredLogCategories(LoggingPropertyMappers.java:443)
    org.keycloak.quarkus.runtime.configuration.mappers.WildcardPropertyMapper.getToFromWildcardTransformer(WildcardPropertyMapper.java:85)
    org.keycloak.quarkus.runtime.configuration.PropertyMappingInterceptor.lambda$appendWildcardsMappedFrom$2(PropertyMappingInterceptor.java:181)
    org.keycloak.quarkus.runtime.configuration.PropertyMappingInterceptor.appendWildcardsMappedFrom(PropertyMappingInterceptor.java:181)
    org.keycloak.quarkus.runtime.configuration.PropertyMappingInterceptor.lambda$iterateNames$0(PropertyMappingInterceptor.java:130)
    org.keycloak.quarkus.runtime.configuration.PropertyMappingInterceptor.iterateNames(PropertyMappingInterceptor.java:153)
    io.smallrye.config.SmallRyeConfig$SmallRyeConfigSourceInterceptorContext.iterateNames(SmallRyeConfig.java:1364)
    io.smallrye.config.SmallRyeConfig$ConfigSources$PropertyNames.latest(SmallRyeConfig.java:1161)
    io.smallrye.config.SmallRyeConfig$ConfigSources$PropertyNames.get(SmallRyeConfig.java:1146)
    io.smallrye.config.SmallRyeConfig.getPropertyNames(SmallRyeConfig.java:676)
    org.keycloak.quarkus.runtime.configuration.Configuration.getPropertyNames(Configuration.java:134)
    org.keycloak.quarkus.runtime.QuarkusSingleProfileConfigResolver.getQuarkusFeatureState(QuarkusSingleProfileConfigResolver.java:24)
    org.keycloak.quarkus.runtime.Environment.getCurrentOrCreateFeatureProfile(Environment.java:203)
    org.keycloak.quarkus.runtime.configuration.AbstractConfigurationTest.createConfig(AbstractConfigurationTest.java:92)
```

`'reproducer.not^ok'` is not a real config property that either Keycloak or
this test declares — it looks like a stray/garbage key surfacing from
`SmallRyeConfig.getPropertyNames()`'s property-name enumeration
(`ConfigSources.PropertyNames.get/latest` → `SmallRyeConfigSourceInterceptorContext.iterateNames`),
which `PropertyMappingInterceptor.iterateNames`/`appendWildcardsMappedFrom`
then walks looking for anything matching a `kc.log-level-*`-shaped wildcard.
`LoggingPropertyMappers.validateLogLevel` correctly rejects it as an invalid
logging category name (it isn't one) — the bug is that this name reached
the validator at all; a real config-source property-name enumeration should
never produce a name like this.

## Scale

2 sub-tests, both in `quarkus/runtime :: LoggingConfigurationTest`
(`jsonEcsFormat`, `fileRotationCustomValues`).

## Next steps

1. Find where `'reproducer.not^ok'`-shaped names actually originate — grep
   Keycloak's config sources/test fixtures for that literal string (it may
   be a real test-only config entry meant to be filtered out by wildcard
   matching logic elsewhere, not literally garbage — worth checking whether
   it exists deliberately in a properties file used by these tests, in which
   case the bug is CratonVM failing to apply whatever filter/exclusion real
   HotSpot applies before this name reaches `validateLogLevel`).
2. Compare `SmallRyeConfig.getPropertyNames()`'s returned name set under
   CratonVM vs. real HotSpot for the exact same test fixture/config, to see
   whether CratonVM surfaces an extra/different name set, or applies
   different iteration-order/dedup logic in
   `PropertyMappingInterceptor.iterateNames`.
3. Possibly related to `quarkus-runtime-logging-wildcard-debug-level-null.md`
   (same class, same wildcard-resolution code path, different failure mode)
   and to the broader `smallrye-config-missing-charset-memorysize-converters.md`
   SmallRye-Config-under-CratonVM divergence — worth a combined re-triage
   once one of these is root-caused.

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.configuration.LoggingConfigurationTest.out.log`
