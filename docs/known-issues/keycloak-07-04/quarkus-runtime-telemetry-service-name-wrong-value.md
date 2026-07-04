# TelemetryConfigurationTest.rootDefaults: telemetry-service-name resolves to a stray placeholder value

Status: open

Date observed: 2026-07-04

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.configuration.TelemetryConfigurationTest#rootDefaults`
(line 12) asserts the default value of config key `telemetry-service-name`
is `"keycloak"`, but gets the literal string `"something3"`:

```
java.lang.AssertionError: Different value for key 'telemetry-service-name'
Expected: is "keycloak"
     but: was "something3"
    org.keycloak.quarkus.runtime.configuration.AbstractConfigurationTest.assertConfig(AbstractConfigurationTest.java:100/104/108)
    org.keycloak.quarkus.runtime.configuration.TelemetryConfigurationTest.rootDefaults(TelemetryConfigurationTest.java:12)
```

`KCRUNNER_RESULT tests=6 failed=1` — the other 5 sub-tests in this class
pass; only the default-value assertion fails.

## Scale

1 sub-test, `quarkus/runtime :: TelemetryConfigurationTest#rootDefaults`.

## Notes

`"something3"` reads like a placeholder/fixture value from a *different*
test case in the same suite leaking through — either a config-source
override left over from a previous test's config build (test-order/state
leak between `AbstractConfigurationTest` config builds), or a genuine
default-value resolution bug picking up the wrong config source's value.
Not yet root-caused; worth checking whether `"something3"` appears
literally anywhere else in this test class or its fixtures (grep the test
source for `"something3"` — if it's a value some OTHER sub-test explicitly
sets, this points at cross-test config-state leakage rather than a
default-resolution bug).

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.configuration.TelemetryConfigurationTest.out.log`
