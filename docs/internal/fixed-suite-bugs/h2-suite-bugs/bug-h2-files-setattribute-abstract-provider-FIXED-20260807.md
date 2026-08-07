# `Files.setAttribute` dispatched to the ABSTRACT `FileSystemProvider.setAttribute` — FIXED 2026-08-07

**Status: ✅ FIXED.** Filed 2026-08-07 as a Windows-only blocker for
`org.h2.test.unit.TestFileSystem`. **The "Windows only" was wrong** — the defect
reproduces identically on Linux; only H2's *branch* into it is
platform-dependent. Corrected and fixed the same day.

## Symptom

```
java.lang.AbstractMethodError: method java/nio/file/spi/FileSystemProvider.setAttribute(
    Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V
    has no Code attribute
        at java.nio.file.Files.setAttribute(Files.java:1764)
        at org.h2.store.fs.disk.FilePathDisk.setReadOnly(FilePathDisk.java:293)
        at org.h2.test.unit.TestFileSystem.testSetReadOnly(TestFileSystem.java:399)
```

## What it was

Not a dispatch bug. `FileSystems.getDefault().provider()` in CratonVM is a
synthetic object *stamped with the abstract class itself* —
`alloc_concurrent_synthetic(ctx, "java/nio/file/spi/FileSystemProvider", 1)` —
and `getClass().getName()` reports `sun.nio.fs.WindowsFileSystemProvider` only
through display remapping. So there is no `sun.nio.fs.AbstractFileSystemProvider`
in the receiver's chain to carry the override, and every abstract method on
`FileSystemProvider` must be answered by a native registration or it is
`AbstractMethodError` by construction.

`readAttributes(Path, String, LinkOption...)` was registered for exactly this
reason (its registration comment says so). `setAttribute` — its write-side twin,
reached by `Files.setAttribute`, `Files.setLastModifiedTime` and everything
name-keyed — was not.

The three-line witness (`SetAttrProbe`, no H2) separates them cleanly:

| call | HotSpot | CratonVM `dev` @ `1ec856c2c` |
|---|---|---|
| `readAttributes(path, Class)` — overridden on the concrete leaf, registered | OK | OK |
| `readAttributes(path, String, …)` — overridden on the abstract mid, registered | OK | OK |
| `setAttribute(path, String, Object, …)` — overridden on the abstract mid, **not** registered | OK | **AbstractMethodError** |

## It was never Windows-only

H2's `FilePathDisk.setReadOnly` picks its route from the FileStore:

```java
if (fileStore.supportsFileAttributeView(PosixFileAttributeView.class)) {
    Files.setPosixFilePermissions(f, permissions);      // Linux — registered, worked
} else if (fileStore.supportsFileAttributeView(DosFileAttributeView.class)) {
    Files.setAttribute(f, "dos:readonly", true);        // Windows — the gap
}
```

So Linux took a different, already-registered route. The witness, which calls
`Files.setAttribute` directly, fails on **both** platforms — measured on Linux
against `dev` @ `1ec856c2c` before the fix. Anything on Linux that calls
`Files.setAttribute` / `Files.getAttribute` was equally broken; it simply had no
reporter.

## The fix

`FileSystemProvider.setAttribute` is registered next to `readAttributes`, and
`write_named_attribute` is screened against the *same* view and name tables the
reader uses, so the two cannot disagree about what a view contains.

The writes delegate rather than reimplement: the DOS flags go through the very
`dos_view_set_*` natives `getFileAttributeView(path, DosFileAttributeView)` hands
to Java callers, and times/permissions through the same path-level helpers those
views use. A later fix to any of them reaches this entry point too.

Classification, split out as `attribute_write_kind` so a unit test can assert it:

| names | behaviour |
|---|---|
| `lastModifiedTime`, `lastAccessTime`, `creationTime` | written (`FileTime`) |
| `readonly`, `hidden`, `archive`, `system` | written (DOS flags) |
| `posix:permissions`, `unix:mode` | written (chmod) |
| `owner`, `group`, `uid`, `gid` | `UnsupportedOperationException` — writable in the JDK, not implemented here, and refused loudly rather than silently doing nothing |
| `size`, `isDirectory`, `fileKey`, `ino`, … | `IllegalArgumentException`, the JDK's own wording for a non-writable name |
| unknown view | `UnsupportedOperationException("View 'x' not available")`, matching the reader |

`NOFOLLOW_LINKS` on a symbolic link is refused rather than silently written
through to the target.

## Verification

`SetAttrProbe` — Linux, `dev` before vs after: `AbstractMethodError` → all steps
`OK`, and the `unix:mode` readback is `33188`, byte-identical to HotSpot's. Not
"no exception": the right value actually landed.

Windows, same probe: `AbstractMethodError` before; after, every step OK with
`dos:readonly=true` read back. The H2 prefixes go from `AbstractMethodError` on
all three to `DONE failed=0` — plain disk 6.4 s, `nioMapped:` 2.4 s,
`split:nioMapped:` 7.0 s. (That is also the first cross-platform confirmation of
the conservative-root fix in the retired `bug-h2-niomapped-unmap-gc-timeout`
write-up; both mapped prefixes had only ever been measured on Linux.)

`probes/ReadOnlyRoundTrip.java` runs the exact `testSetReadOnly` sequence with no
H2 in it — create, mark read-only, assert `canWrite()` is false, delete — and is
what established that H2 needs `Files.setAttribute` TWICE on Windows: deleting a
read-only file there raises `AccessDeniedException`, and `FilePathDisk.delete`
catches it and calls `Files.setAttribute(file, "dos:readonly", false)` before
retrying. Both halves were the same missing registration.

### One divergence this surfaced, not fixed here

On HotSpot/Windows `Files.deleteIfExists` on a read-only file throws
`AccessDeniedException`; on CratonVM/Windows it deletes the file. The DOS bit is
genuinely set (`Files.getAttribute` reads back `true`, and `File.canWrite()` is
`false`), so this is the delete path ignoring an attribute Windows enforces. It
does not affect `testSetReadOnly` — H2 wants the file gone and it is — but it
means H2's `AccessDeniedException` recovery branch never runs here, and a program
relying on read-only protection would be surprised.

`the_writable_attributes_are_the_ones_the_jdk_lets_you_write` pins the
classification against the reader's tables, so a name added to
`attribute_names_for_view` without a write verdict fails the build rather than
producing "`readAttributes` returned it but `setAttribute` says it does not
exist".

## What this does NOT fix: non-default providers

A native registered on the abstract class answers for EVERY provider, not just
the default one. `probes/ZipAttrProbe.java` opens a `jar:` filesystem and works
on an entry:

| | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `fs.provider()` | `jdk.nio.zipfs.ZipFileSystemProvider` | `sun.nio.fs.UnixFileSystemProvider` | same |
| `readAttributes(entry, "basic:size")` | `{size=5}` | `NoSuchFileException` | same |
| `setAttribute(entry, "lastModifiedTime", …)` | OK | `AbstractMethodError` | `IOException` |

So zipfs entries were already being served by the default provider's name-keyed
reader before this change — the hijack is the existing design (one synthetic
provider object, name-keyed natives on the abstract class), not something this
adds. What changes is the *shape* of the zipfs `setAttribute` failure, from
`AbstractMethodError` to a JDK-legal `IOException` on a path that does not exist.
Both are wrong; neither writes anywhere real, because the synthetic `JARFS…`
path never resolves. Filed separately.

## Note for whoever picks this up

The registration is a workaround for the shape, not a cure. Every abstract
method on `java.nio.file.spi.FileSystemProvider` is one unregistered native away
from the same `AbstractMethodError`, because the default-provider object is
stamped with the abstract class rather than a concrete provider. The durable fix
is to stamp it with a real concrete provider class; until then, the registration
list next to `readAttributes`/`setAttribute` IS the provider's method table, and
anything added to `FileSystemProvider` upstream has to be added there too.
