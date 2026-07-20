# `module/spring-boot-health`: `DiskSpaceHealthIndicator` never reports DOWN because `File.getUsableSpace()`/`getFreeSpace()`/`getTotalSpace()` are hardcoded stubs

**Status: FIXED — 2026-07-20** (found 2026-07-17)

This module contributed 5 non-passing classes to the 2026-07-17 rerun triage
batch. 4 of them (`HealthEndpointTests`, `ReactiveHealthIndicatorImplementationTests`,
`AbstractHealthIndicatorTests`, `AbstractReactiveHealthIndicatorTests`) were
the same cross-module `CapturedOutput`-always-empty signature filed in
[`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md)
(see its "Update 2026-07-17 (bin10 rerun triage)" section) — already fixed
2026-07-18 per that doc's update, re-verified still passing (JIT on and off)
in this session's binary. This doc's own bug was the 5th, an unrelated,
distinct bug in `DiskSpaceHealthIndicatorTests`.

## `DiskSpaceHealthIndicatorTests` — 1/3 tests fail

```
JUnit Jupiter:DiskSpaceHealthIndicatorTests:whenPathDoesNotExistDiskSpaceIsDown()
    => org.opentest4j.AssertionFailedError:
expected: DOWN
 but was: UP
       org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown(DiskSpaceHealthIndicatorTests.java:96)
```

## Root cause 1: `java.io.File` disk-space natives were hardcoded `i64::MAX` stubs

`DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown` calls
`new DiskSpaceHealthIndicator(new File("does/not/exist"), THRESHOLD).health()`.
`DiskSpaceHealthIndicator.doHealthCheck`
(`apps/spring-boot/module/spring-boot-health/src/main/java/org/springframework/boot/health/application/DiskSpaceHealthIndicator.java:60-75`)
reports `DOWN` iff `this.path.getUsableSpace() < threshold.toBytes()`. On
real HotSpot, `File.getUsableSpace()` for a non-existent path returns `0`,
which is `< threshold` (1024 bytes here) → `DOWN`. On CratonVM,
`java.io.File.getUsableSpace()`, `.getFreeSpace()`, and `.getTotalSpace()`
were all hardcoded native stubs that returned `i64::MAX` **unconditionally**,
regardless of whether the path existed. Since `i64::MAX >= threshold.toBytes()`
is always true, `doHealthCheck` always took the `builder.up()` branch — the
indicator could **never** report `DOWN`, for any path, existent or not, full
disk or empty. The other 2 tests in the class (`diskSpaceIsUp`/`diskSpaceIsDown`)
passed regardless because they mock `File` directly with Mockito (`@Mock
File fileMock`, stubbing `getUsableSpace()` explicitly) and never reach this
native stub at all.

**Fix:** replaced the three stubs in `native-builtins/src/phases_late.rs`
(`register_phase57_file`) with a real OS disk-space query — a new
`file_disk_space_bytes(path) -> Option<(total, free, usable)>` helper that
returns `None` (callers report `0`) when `path` does not name an existing
file/directory, matching HotSpot's contract. On Windows it resolves the
containing directory (the path itself if it's already a directory, else its
parent) and calls `GetDiskFreeSpaceExW` via a local `extern "system"` block
(same pattern already used elsewhere in this file for `GetFullPathNameW`,
no new crate dependency); on Unix it calls `libc::statvfs` directly on the
path.

## Root cause 2 (found while verifying fix 1): `FileSystem.getSpace(File, int)` had its own, separate hardcoded stub

Fixing the `java/io/File`-level natives alone was **not sufficient** —
`whenPathDoesNotExistDiskSpaceIsDown` still failed (`usable=100000000000`,
a suspiciously round 100GB) even after fix 1. Root cause: real-JDK
`File.getUsableSpace()`/`getFreeSpace()`/`getTotalSpace()` bytecode normally
delegates to a package-private native, `java.io.FileSystem.getSpace(File,
int)` (`SPACE_TOTAL=0`/`SPACE_FREE=1`/`SPACE_USABLE=2`) — but this delegation
is invisible on CratonVM by default because the natives registered directly
on `java/io/File` itself intercept the call **before** that bytecode ever
runs. `DiskSpaceHealthIndicatorTests` also has `@Mock private File fileMock;`
under `@ExtendWith(MockitoExtension.class)`: Mockito's inline mock maker
retransforms the **entire `java.io.File` class** via
`Instrumentation.redefineClasses` the moment any test in the class needs a
mock instance (`@BeforeEach` runs for every test method, including the
non-mock one) — this is process-wide, not scoped to the one mocked field. Once
`java.io.File` is redefined, CratonVM no longer treats its methods as
directly native; the real (redefined) bytecode runs for **every** `File`
instance in the process, including the genuinely real
`new File("does/not/exist")` in the third test — and that bytecode's
`getUsableSpace()`/etc. call through to `FileSystem.getSpace(File, int)`,
registered separately on `java/io/WinNTFileSystem`/`java/io/UnixFileSystem`
(`native-builtins/src/phases_late.rs`, in the `for fs_cls in &["java/io/WinNTFileSystem",
"java/io/UnixFileSystem"]` loop) — which had its own unconditional `100GB`
stub, untouched by fix 1.

**Fix:** rewrote `getSpace(File, int)` to call the same `file_disk_space_bytes`
helper and dispatch on the `int` space-type argument (`0`=total, `1`=free,
`2`=usable). This is a real, general fix — any suite where a test class mixes
a Mockito-mocked `File` with a genuinely real one in the same process will
hit this same path, not just this one test.

## Verification (2026-07-20, binary `cratonvm-diskspace-health-20260720.exe`, worktree `C:\craton\CratonVM-diskspace-health-20260720`, branch `fix/springboot-diskspace-health-20260720`)

- `DiskSpaceHealthIndicatorTests` 3/3 PASS, both `-Jit on` and `-Jit off`.
- The 4 `CapturedOutput` classes this doc originally listed as a residual of a
  different cluster re-verified 3/3, 27/27, 6/6, 6/6 PASS respectively.
- Full `module/spring-boot-health` suite (66 classes, all subtrees) re-run
  clean: 62 PASS, 4 `EMPTY` (abstract `Abstract*Tests`/`*SupportTests` base
  classes with no directly-runnable `@Test` methods — expected, not a
  regression), 0 FAIL.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-health` | `org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests` |
