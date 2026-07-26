# IgnoredArtifactsTest.multipleDatasources datasource properties missing

Status: fixed

Date observed: 2026-07-04
Date fixed: 2026-07-05

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.configuration.IgnoredArtifactsTest#multipleDatasources`
failed because `quarkus.datasource.dog-store.db-kind` and
`quarkus.datasource.cat-store.db-kind` were absent from the SmallRye config.

## Root Cause

There were two path/setup problems:

1. The suite runner launched module-scoped tests from the Keycloak repository
   root, but these tests call `Environment.setHomeDir(Paths.get("src/test/resources/"))`.
   The relative path must resolve from `quarkus/runtime`.
2. On Windows, `File.toPath()` allocated a synthetic `Path` with Windows
   display separators. `Path.toUri()` then emitted `file:///C:%5C...`, so
   SmallRye's `AbstractLocationConfigSourceLoader` treated the file URI as a
   non-regular path and skipped `conf/quarkus.properties`.

## Fix

- `../../../../apps/keycloak-suite-runner/run-keycloak-suite.ps1` now uses the test
  module root as the process working directory when the class row has a module.
- `java.io.File.toPath()` now allocates through the common Path allocator, so
  Windows paths use the same internal slash canonicalization as `Paths.get(...)`.

## Validation

- Linux remote no-JIT: `IgnoredArtifactsTest` passed all 15 tests.
- Windows JDK 25 no-JIT: `IgnoredArtifactsTest` passed all 15 tests in
  `kc-quarkus-config-three-winprobe-20260705-002`.
- Focused Windows probe: `FileToPathUriWindowsProbe` now prints
  `file:///C:/.../test_classes/FileToPathUriWindowsProbe.java` under both
  HotSpot JDK 25 and CratonVM.
