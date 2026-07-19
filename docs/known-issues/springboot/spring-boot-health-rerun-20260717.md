# `module/spring-boot-health`: `DiskSpaceHealthIndicator` never reports DOWN because `File.getUsableSpace()`/`getFreeSpace()`/`getTotalSpace()` are hardcoded stubs

**Status: OPEN — found 2026-07-17**

This module contributed 5 non-passing classes to the 2026-07-17 rerun triage
batch. 4 of them (`HealthEndpointTests`, `ReactiveHealthIndicatorImplementationTests`,
`AbstractHealthIndicatorTests`, `AbstractReactiveHealthIndicatorTests`) are
the same cross-module `CapturedOutput`-always-empty signature already filed
in [`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md)
(see its "Update 2026-07-17 (bin10 rerun triage)" section) — not repeated
here. This doc covers the 5th, an unrelated, distinct, fully-confirmed bug.

## `DiskSpaceHealthIndicatorTests` — 1/3 tests fail

```
JUnit Jupiter:DiskSpaceHealthIndicatorTests:whenPathDoesNotExistDiskSpaceIsDown()
    => org.opentest4j.AssertionFailedError:
expected: DOWN
 but was: UP
       org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown(DiskSpaceHealthIndicatorTests.java:96)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-health.org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests.out.log`

## Root cause (CONFIRMED at file:line precision)

`DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown` calls
`new DiskSpaceHealthIndicator(new File("does/not/exist"), THRESHOLD).health()`.
`DiskSpaceHealthIndicator.doHealthCheck` (this worktree,
`apps/spring-boot/module/spring-boot-health/src/main/java/org/springframework/boot/health/application/DiskSpaceHealthIndicator.java:60-75`)
reports `DOWN` iff `this.path.getUsableSpace() < threshold.toBytes()`. On
real HotSpot, `File.getUsableSpace()` for a non-existent path returns `0`,
which is `< threshold` (1024 bytes here) → `DOWN`. On CratonVM,
`java.io.File.getUsableSpace()`, `.getFreeSpace()`, and `.getTotalSpace()`
are all hardcoded native stubs that return `i64::MAX` **unconditionally**,
regardless of whether the path exists:

`native-builtins/src/phases_late.rs:16626-16635`:
```rust
// --- Disk space (fallback: return i64::MAX when no OS query is available) ---
r.register(file, "getFreeSpace", "()J", |_ctx, _args| {
    Ok(Some(Value::Long(i64::MAX)))
});
r.register(file, "getTotalSpace", "()J", |_ctx, _args| {
    Ok(Some(Value::Long(i64::MAX)))
});
r.register(file, "getUsableSpace", "()J", |_ctx, _args| {
    Ok(Some(Value::Long(i64::MAX)))
});
```

Since `i64::MAX >= threshold.toBytes()` is always true, `doHealthCheck`
always takes the `builder.up()` branch — the indicator can **never** report
`DOWN`, for any path, existent or not, full disk or empty. The other 2 tests
in the class (`diskSpaceIsUp`/`diskSpaceIsDown`) pass only because they mock
`File` directly with Mockito (`@Mock File fileMock`, stubbing
`getUsableSpace()` explicitly) and never reach this native stub at all.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-health` | `org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests` |
