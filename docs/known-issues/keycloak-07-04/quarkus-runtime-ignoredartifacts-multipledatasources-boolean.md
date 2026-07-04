# IgnoredArtifactsTest.multipleDatasources: datasource-ignore detection returns false instead of true

Status: open

Date observed: 2026-07-04

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.configuration.IgnoredArtifactsTest#multipleDatasources`
(line 120) asserts a boolean condition is `true`, gets `false`:

```
java.lang.AssertionError:
Expected: is <true>
     but: was <false>
    org.keycloak.quarkus.runtime.configuration.IgnoredArtifactsTest.multipleDatasources(IgnoredArtifactsTest.java:120)
```

`KCRUNNER_RESULT tests=15 failed=1` — the other 14 sub-tests in this class
pass; only this one boolean assertion fails.

## Scale

1 sub-test, `quarkus/runtime :: IgnoredArtifactsTest#multipleDatasources`.

## Notes

Not yet root-caused. The test name and surrounding class (`IgnoredArtifactsTest`)
suggest this checks that some datasource-artifact-ignoring logic correctly
identifies multiple configured datasources as a specific ignorable/handled
case; under CratonVM the check returns `false` where `true` is expected.
Needs the actual test source (`IgnoredArtifactsTest.java:120` and its
surrounding setup) read to determine whether this traces to a config-parsing
divergence (plausibly related to the broader SmallRye-Config-under-CratonVM
issues documented alongside this one) or a different, narrower datasource-
detection defect.

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.configuration.IgnoredArtifactsTest.out.log`
