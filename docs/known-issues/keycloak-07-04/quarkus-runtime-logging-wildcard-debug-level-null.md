# LoggingConfigurationTest.testWildcardOptionFromConfigFile: wildcard log level resolves to null instead of DEBUG

Status: open

Date observed: 2026-07-04

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.configuration.LoggingConfigurationTest#testWildcardOptionFromConfigFile`
(line 364) asserts a wildcard-configured log category resolves to level
`DEBUG`, but gets `null`:

```
java.lang.AssertionError: expected:<DEBUG> but was:<null>
    org.keycloak.quarkus.runtime.configuration.LoggingConfigurationTest.testWildcardOptionFromConfigFile(LoggingConfigurationTest.java:364)
```

This is a single isolated sub-test failure — the other 26 sub-tests in this
class pass.

## Scale

1 sub-test, `quarkus/runtime :: LoggingConfigurationTest` (26/29 sub-tests
pass in the same class; 2 other sub-tests fail with a different, unrelated
signature — see `quarkus-runtime-logging-getpropertynames-garbage-key.md`).

## Notes

Not yet root-caused. The test sets a wildcard log-category level via a
config file (`kc.log-level-<category>=DEBUG`-shaped wildcard property, per
the test name) and expects `LoggingPropertyMappers`/the config mapper
resolution to resolve that wildcard for the specific category under test —
under CratonVM this resolves to no value (`null`) rather than the
configured `DEBUG`. Given the sibling
`quarkus-runtime-logging-getpropertynames-garbage-key.md` finding shows a
different, confirmed-garbage property name coming back from
`SmallRyeConfig.getPropertyNames()` in the very same wildcard-resolution
code path (`LoggingPropertyMappers.getConfiguredLogCategories` →
`WildcardPropertyMapper.getToFromWildcardTransformer` →
`PropertyMappingInterceptor.iterateNames`), these two are plausibly the same
underlying property-name-enumeration defect manifesting two different ways
(one throws on a garbage name, this one silently fails to match the real
wildcard and returns null) — worth re-checking together.

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.configuration.LoggingConfigurationTest.out.log`
