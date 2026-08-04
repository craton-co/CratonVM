# nio.file write path silently ignores LinkOption.NOFOLLOW_LINKS (writes through a symlink instead of throwing)

**Status: OPEN — found 2026-08-04**

## Symptom

`org.springframework.boot.system.ApplicationPidTests#whenSymlinkToTargetExistsAtPidFileLocationWriteThrows`
fails:

```
java.lang.AssertionError:
Expecting code to raise a throwable.
    org.springframework.boot.system.ApplicationPidTests.whenSymlinkToTargetExistsAtPidFileLocationWriteThrows(ApplicationPidTests.java:110)
```

The test creates a regular file `target`, a symbolic link `link -> target`,
then calls `ApplicationPid.write(link)` and asserts it throws an
`IOException`. On CratonVM, no exception is thrown — the write silently
succeeds.

## Root cause

`ApplicationPid.write(File)`
(`core/spring-boot/.../system/ApplicationPid.java:109-118`) does:

```java
Files.writeString(path, this.pid.toString(), StandardOpenOption.TRUNCATE_EXISTING,
        StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS);
```

`LinkOption.NOFOLLOW_LINKS` is passed as one of the `OpenOption` varargs.
Per spec, when the target path's last component is itself a symbolic link,
opening it with `NOFOLLOW_LINKS` must fail (real JDK/HotSpot on Linux uses
`O_NOFOLLOW`, which returns `ELOOP` for a symlink, surfacing as an
`IOException`).

CratonVM's native output-stream open path
(`native-builtins/src/phases_late/nio_file.rs`, `fsp_scan_open_options` /
`fsp_new_output_stream`, roughly lines 7529-7620 and 7744-7770) only scans
the `OpenOption[]`/`Set<OpenOption>` for `APPEND` and `CREATE_NEW`:

```rust
if n.eq_ignore_ascii_case("APPEND") { append = true; }
if n.eq_ignore_ascii_case("CREATE_NEW") { create_new = true; }
```

`NOFOLLOW_LINKS` is never inspected here, and `open_write_gated` (via
`capability_gate.rs`) opens the path with ordinary `OpenOptions`, which
follow symlinks by default. So a write requested with `NOFOLLOW_LINKS`
against a path that is itself a symlink silently follows the link and
writes through to the target instead of failing with `ELOOP`/`IOException`.

Note that `NOFOLLOW_LINKS` **is** correctly honored elsewhere in the same
file for *read-only* / metadata operations — `p57_link_options_nofollow`,
used by `Files.exists`, attribute reads, `isSymbolicLink`, etc. (see hits at
lines ~8987, ~9004, ~9021, ~9037, ~14234). The gap is specific to the
*write*/open-for-output path (`fsp_scan_open_options` /
`fsp_new_output_stream`, and likely the sibling `newByteChannel`/
`newFileChannel` write paths), which never consults it.

## Affected classes

- `core/spring-boot` — `org.springframework.boot.system.ApplicationPidTests`
  (`whenSymlinkToTargetExistsAtPidFileLocationWriteThrows`)

Likely also affects any other test that opens a file for writing through a
symlink while passing `LinkOption.NOFOLLOW_LINKS` as an `OpenOption`.
