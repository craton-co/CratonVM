# jarmode-tools: preserve `ExtractLayersCommand` entry timestamps

**Status: FIXED — 2026-07-18**

## Root cause

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

## Fix

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

## Regression coverage and validation

`native-io/src/zip_real_jar.rs` has a unit regression for a local `0x5455`
record containing all three timestamps; the focused test passes.

Focused real-fixture validation used JDK 25.0.3 and the unique binary
`C:\craton\cratonvm-jarmode-timestamps-20260718-019f742a.exe`:

| VM mode | Result |
|---|---|
| HotSpot | `ExtractLayersCommandTests`: 6/6 pass |
| CratonVM JIT | `ExtractLayersCommandTests`: 6/6 pass (24.0 s) |
| CratonVM `--nojit` | `ExtractLayersCommandTests`: 6/6 pass (26.5 s) |

## Affected class

| Module | Class |
|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractLayersCommandTests` |
