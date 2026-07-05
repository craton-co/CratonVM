# Bug 12: MSC StabilityMonitor awaitStability returned null controllers

Status: FIXED
Date found: 2026-07-05
Area: WildFly domain tests, JBoss MSC native bridge

## Symptom

`org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase` aborted before real test reporting with:

```text
Cannot invoke "org.jboss.msc.service.ServiceController.provides()" because "controller" is null
```

The failing call was from WildFly's service verification path after `StabilityMonitor.awaitStability(Set, Set)` populated the caller-provided failed/problem sets.

## Root Cause

CratonVM had native `awaitStability` cleanup for `ServiceContainer`, but the domain smoke test calls `org.jboss.msc.service.StabilityMonitor.awaitStability(...)`. That path was still running the unfiltered model and could leave null slots in MSC failed/problem sets. Later WildFly code iterated those sets and dereferenced a null `ServiceController`.

## Fix

`native-builtins/src/jboss_msc.rs` now registers the same await-stability native bridge for `StabilityMonitor`, including the overloads with timeout and `StabilityStatistics`. The helper also accepts both `lock` and `stabilityLock` field names and copies failed/problem sets without null entries.

## Verification

Focused no-JIT rerun:

```text
/data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner/out/azure-defaultconfig-stabmon-nojit-076-nojit-real-failed-20260705-150207
```

The previous `ServiceController.provides()` null-controller signature is absent after the patch; execution advanced to Surefire channel reporting.
