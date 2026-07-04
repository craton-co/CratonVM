# quarkus/runtime: three isolated config-resolution value divergences

Status: open — low volume, likely related to the Charset/MemorySize converter gap

Date observed: 2026-07-04

## Summary

Three `quarkus/runtime` classes each fail exactly one sub-test with a wrong
resolved config value (not a crash — the class runs, most sub-tests pass,
one or a few assert a specific value and get something else):

1. **`LoggingConfigurationTest`** (3 of its sub-tests fail): asserts a log
   level of `DEBUG` but gets `null`. Traced through
   `LoggingPropertyMappers.parseRootLogLevel` — a config property that should
   resolve to a level enum comes back unset.
2. **`TelemetryConfigurationTest`** (1 of 6 sub-tests fails): asserts config
   key `telemetry-service-name` equals `"keycloak"`, actual value is the
   literal string `"something3"` — looks like a stray placeholder/default
   value leaking through, or override-precedence resolving the wrong config
   source.
3. **`IgnoredArtifactsTest.multipleDatasources`** (1 of 15 sub-tests fails):
   asserts a boolean `true`, gets `false` — datasource-ignore detection logic.

## Why these are filed together

All three are narrow (single-digit sub-test counts out of otherwise-passing
classes) config-value mismatches in the same module
(`quarkus/runtime`) that also hosts the `SmallRyeConfig.getConfigMapping`
1-arg bug (fixed, see `docs/internal/fixed-suite-bugs/
smallrye-getconfigmapping-1arg-bare-interface-abstractmethoderror.md`) and is
downstream of the Charset/MemorySize converter gap
(`smallrye-config-missing-charset-memorysize-converters.md`). They may turn
out to be symptoms of the same broader SmallRye-Config-under-CratonVM
divergence rather than three separate defects — recommend re-running this
trio after the converter gap is fixed before investigating each in isolation.

## Next steps

Re-triage after `smallrye-config-missing-charset-memorysize-converters.md` is
resolved; if these three still fail identically, each becomes its own
focused investigation (they are unrelated to each other on the surface —
logging level, telemetry service name, and datasource-ignore detection touch
different config subsystems).

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.{configuration.LoggingConfigurationTest,configuration.TelemetryConfigurationTest,configuration.IgnoredArtifactsTest}.out.log`
