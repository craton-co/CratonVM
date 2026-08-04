# jarmode-tools: preserve `ExtractLayersCommand` entry timestamps

**Status: OPEN — REGRESSED 2026-08-04.** Originally fixed 2026-07-18
(verified `ExtractLayersCommandTests` 6/6 on HotSpot, CratonVM JIT, and
CratonVM `--nojit`, unique binary
`C:\craton\cratonvm-jarmode-timestamps-20260718-019f742a.exe`). The
`craton-residual32-20260804` rerun (source: `residual-azure-20260802-32.tsv`,
run against `/data/data/springboot-jsonreader-deprecation-20260718`, JDK
`25.0.3`) shows the same class failing again, plus a previously-unlisted
sibling class hitting the identical mechanism.

## Root cause (original, 2026-07-18)

`ExtractLayersCommandTests` creates archive entries with creation, access, and
last-modified `FileTime` values, then verifies that extraction preserves the
last-modified value. CratonVM's active Spring Boot `JarFile.entries()` bridge
cached only the name, sizes, method, CRC, and content bytes. The synthetic
`JarEntry` therefore had no `ZipEntry.mtime`, so `getLastModifiedTime()` fell
back to the archive's DOS timestamp. For these JDK-written entries that
fallback is `1980-01-01` (`315446400` Unix seconds), rather than the expected
extended timestamp.

The lower-level real ZIP bridge had the same metadata omission. In addition,
the native `BasicFileAttributeView.setTimes` implementation was a no-op, so
even a correctly materialized source time could not reach the extracted file.

## Original fix (2026-07-18)

- Parse ZIP extended timestamp (`0x5455`) and NTFS (`0x000a`) extra fields,
  including the local-header fields that carry access and creation times.
- Preserve those values in Spring Boot's cached `JarEntryRec` and materialize
  `mtime`, `atime`, and `ctime` on synthetic `JarEntry` objects.
- Materialize the same fields for the regular `JarFile`/`ZipFile` bridge.
- Implement `BasicFileAttributeView.setTimes` / `DosFileAttributeView.setTimes`
  with real host filesystem updates; on Windows this uses `SetFileTime` to
  preserve all three supplied values.
- Extend the ignored Spring Boot fixture's `SbRunner` to print each original
  failure throwable and its suppressed assertion detail. This exposed the
  decisive `expected … but was 315446400` difference during diagnosis.

`native-io/src/zip_real_jar.rs` has a unit regression for a local `0x5455`
record containing all three timestamps; the focused test passes (this was
not re-checked as part of today's regression triage).

## Regression note (2026-08-04)

Today's residual rerun (`craton-residual32-20260804`, shards `s1`) shows
`ExtractLayersCommandTests` failing again, and a related class
(`ExtractCommandTests`) hitting what is very likely the same mechanism for
the first time:

| Class | Result | Failing test(s) |
|---|---|---|
| `ExtractLayersCommandTests` | 3/6 failed | `runExtractsLayers()`, `runWhenHasDestinationOptionExtractsLayers()`, `runWhenHasLayerParamsExtractsLimitedLayers()` — all three, and only these three, call the private `timeAttributes(File)` helper that asserts `Files.getFileAttributeView(file, BasicFileAttributeView.class).readAttributes().lastModifiedTime()` equals `LAST_MODIFIED_TIME`. The other 3 methods in the class (which don't check timestamps) pass. |
| `ExtractCommandTests` | 1/22 failed | `Extract$appliesFileTimes()` (`ExtractCommandTests.java:147`) — asserts the same `lastModifiedTime()` invariant (`fileTimeAttributes`) plus `ZipEntry` timestamp equality between the extracted copy and the source archive (`entryTimeAttributes`), via the plain (non-layered) `ExtractCommand` path. Not in the original 2026-07-18 affected-classes list — either it wasn't covered by that round's verification, or this is new fallout from later changes to the same code path. |

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s1/all-jit/logs/loader_spring-boot-jarmode-tools.org.springframework.boot.jarmode.tools.ExtractLayersCommandTests.{out,err}.log`
`apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s1/all-jit/logs/loader_spring-boot-jarmode-tools.org.springframework.boot.jarmode.tools.ExtractCommandTests.{out,err}.log`

The runner's log format for this rerun does not capture the AssertJ
`MultipleFailuresError`'s nested per-assertion messages (only the top-level
exception class and a `MethodSource` line), so the exact
"expected/but-was" values were not re-captured this round — only the
mechanism (same test helper, same assertion, same class family) is
confirmed via test-source inspection.

**Suspect regression window:** `native-io/src/zip_real_jar.rs` and
`native-builtins/src/phases_late/nio_file.rs` (the two files this bug's
original fix touched) were both substantially reworked by
`968572d757` ("fix(springboot): close loader ZIP residual shard", 2026-07-28,
10 days after this bug's fix) — in particular `alloc_zip_entry`'s handling of
which fields get dual-written depends on a new `real_layout` check
(whether the `ZipEntry` class resolves an `xdostime` field), and
`zip_entry_times`/`ZipEntryTimes` gained a `dos_time` field threaded through
several call sites. This is a plausible place for the `mtime`/`atime`/`ctime`
materialization to have silently stopped reaching the extracted files again,
but it was **not confirmed** by reading current behavior line-by-line this
round — flagged for the next investigation pass rather than asserted as the
confirmed cause. `set_file_attribute_times` itself
(`native-builtins/src/phases_late/nio_file.rs:13455`, using the `filetime`
crate on non-Windows) still looks correct on inspection, so if this is the
same defect family, the more likely break point is upstream of it — entries
no longer carrying a populated `mtime`/`atime`/`ctime` `FileTime` object by
the time `ExtractCommand`/`ExtractLayersCommand` reads
`ZipEntry.getLastModifiedTime()` to drive the `setTimes` call.

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractLayersCommandTests` |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractCommandTests` (added 2026-08-04, `Extract$appliesFileTimes()` only — same mechanism, not confirmed as the identical defect) |
