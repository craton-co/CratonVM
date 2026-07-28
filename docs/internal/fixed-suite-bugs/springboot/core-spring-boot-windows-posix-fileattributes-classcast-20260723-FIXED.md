# Windows POSIX file-attributes request contract fixed

**Status: FIXED 2026-07-28**

## Root cause and fix

The native bridges for both `Files.readAttributes(Path, Class, LinkOption[])`
and `FileSystemProvider.readAttributes(Path, Class, LinkOption[])` ignored the
requested attribute interface. On Windows they therefore returned a Windows
attributes object even when the caller requested `PosixFileAttributes`. The
original Spring Boot failure was a `ClassCastException` from
`Files.getPosixFilePermissions`; after a narrow interim guard it could instead
appear as a false successful generic read. Neither result matched HotSpot.

The bridges now inspect their `Class` argument. On Windows they allow only the
implemented `BasicFileAttributes` and `DosFileAttributes` interfaces and throw
`UnsupportedOperationException` for POSIX and other unsupported interfaces.
The link-options argument is also passed through to the shared attributes
builder, preserving `NOFOLLOW_LINKS` semantics for the generic entry points.

## Validation

Built on Windows from the isolated `origin/dev` worktree with a dedicated
Cargo target and a uniquely retained executable. A direct real-JDK probe
verified `BasicFileAttributes` and `DosFileAttributes` reads, then verified
that `Files.readAttributes(path, PosixFileAttributes.class, ...)` throws
`UnsupportedOperationException` under CratonVM with JIT enabled and with
`--nojit`.

The affected real Spring Boot classes passed in both modes:

| Class | JIT | `--nojit` |
|---|---:|---:|
| `org.springframework.boot.system.ApplicationPidTests` | 11 started, 0 failed, 2 skipped | 11 started, 0 failed, 2 skipped |
| `org.springframework.boot.context.ApplicationPidFileWriterTests` | 9 started, 0 failed | 9 started, 0 failed |

Each runner result reported `SBRUNNER_RESULT` with zero failed, aborted, and
failed containers. The separate no-JIT rerun was intentional: the runner's
parallel all-modes launch contended on its shared pathing-JAR output on this
Windows host before the no-JIT worker started.
