# jarmode-tools: `ExtractLayersCommand` extracted-file timestamps don't match source jar entry (UNCONFIRMED)

**Status: OPEN — found 2026-07-17. Hypothesis only, not confirmed.**

## Symptom

`ExtractLayersCommandTests` — 3 of 6 tests fail, all with
`org.assertj.core.error.AssertJMultipleFailuresError` and no further detail
captured by the suite runner's log format (the nested multi-failure detail
that `MultipleFailuresError` normally carries was not dumped into either the
`.out.log` or `.err.log` for this run):

```
=> org.assertj.core.error.AssertJMultipleFailuresError
   org.opentest4j.MultipleFailuresError.<init>(MultipleFailuresError.java:51)
   org.springframework.boot.jarmode.tools.ExtractLayersCommandTests.runWhenHasLayerParamsExtractsLimitedLayers(ExtractLayersCommandTests.java:140)
   ...
   org.springframework.boot.jarmode.tools.ExtractLayersCommandTests.runWhenHasDestinationOptionExtractsLayers(ExtractLayersCommandTests.java:129)
   ...
   org.springframework.boot.jarmode.tools.ExtractLayersCommandTests.runExtractsLayers(ExtractLayersCommandTests.java:101)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-jarmode-tools.org.springframework.boot.jarmode.tools.ExtractLayersCommandTests.out.log`

## Hypothesis (not confirmed — honest gap)

All three failing test methods share a helper,
`timeAttributes(File)`, that every one of them calls after extraction:

```java
// ExtractLayersCommandTests.java
private static final FileTime LAST_MODIFIED_TIME = FileTime.from(NOW.minus(2, ChronoUnit.DAYS));
...
private void timeAttributes(File file) {
    BasicFileAttributes basicAttributes = Files
        .getFileAttributeView(file.toPath(), BasicFileAttributeView.class)
        .readAttributes();
    assertThat(basicAttributes.lastModifiedTime().to(TimeUnit.SECONDS))
        .isEqualTo(LAST_MODIFIED_TIME.to(TimeUnit.SECONDS));
}
```

`ExtractLayersCommand`'s production code
(`ExtractCommand.java:283`/`:302`/`:318`, shared with the sibling
`extract` command) round-trips each `JarEntry`'s last-modified time onto the
extracted file via
`Files.getFileAttributeView(path, ...).setTimes(sourceAttributes.lastModifiedTime(), ...)`.
The 3 failing tests all assert this round-trip landed correctly; the 3
passing tests in the same class (`shouldExtractSelectedLayers`,
`runWithApplicationEntryWithoutLibraries`,
`runWhenHasApplicationDestinationOptionExtractsLayersAndApplication` — names
inferred from the class, not individually confirmed passing/failing beyond
the aggregate `3 successful / 3 failed` count) do not call `timeAttributes`.

This is consistent with — but not proof of — a timestamp-preservation gap
somewhere in the `JarEntry.getLastModifiedTime()` → DOS-time decode path, or
in `BasicFileAttributeView.setTimes`'s native filesystem call on Windows,
that either doesn't set the modification time at all or sets it to the
wrong value. **This session did not confirm the mechanism**: the
`AssertJMultipleFailuresError`'s actual per-assertion diffs (which would show
whether it's the DOS-time decode, the `setTimes` native, or something else
entirely, e.g. the `containsOnly(...)` file-listing assertions on the same
lines) were not captured in the runner's log output, and no source-level
native-registration issue was found to pin this to a specific file:line in
the time available.

**What would confirm/refute this:** rerun just this class with a JUnit
console listener that dumps full `MultipleFailuresError` detail (or capture
the failure via a standalone repro:
`Files.setLastModifiedTime`/`getFileAttributeView(...).setTimes(...)`
round-trip on a real file, compared against HotSpot), to see whether the
mismatch is in the DOS-time decode, the native `setTimes` call, or an
unrelated assertion (e.g. `extract.list()` directory-listing contents) on
the same statement lines.

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractLayersCommandTests` (3 of 6 test methods) |
