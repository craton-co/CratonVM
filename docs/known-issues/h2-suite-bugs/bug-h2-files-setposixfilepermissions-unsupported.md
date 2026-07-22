# `java.nio.file.Files.setPosixFilePermissions` throws `UnsupportedOperationException` under real-JDK mode on Linux

## Status
**OPEN** — new finding, 2026-07-21.

## Severity
**MEDIUM** — any code that sets POSIX file permissions via the standard
`java.nio.file` API (rather than legacy `File.setReadOnly()`/`chmod`
shelling out) fails on a platform (Linux) where this should always be
supported.

## Affected test classes
Both PASS on the HotSpot JDK25 baseline, and both fail with the identical
stack:
- `org.h2.test.unit.TestFileSystem` (`testSetReadOnly`)
- `org.h2.test.unit.TestTraceSystem` (`testReadOnly`)

## Symptom
```
java.lang.UnsupportedOperationException
	at java/nio/file/Files.setPosixFilePermissions(Files.java:1987)
	at org/h2/store/fs/disk/FilePathDisk.setReadOnly(FilePathDisk.java:291)
	at org/h2/store/fs/FileUtils.setReadOnly(FileUtils.java:330)
```
H2's `FilePathDisk.setReadOnly()` calls
`Files.setPosixFilePermissions(path, EnumSet.of(PosixFilePermission.OWNER_READ, ...))`
to mark a file read-only on Unix-like systems. Real JDK's
`Files.setPosixFilePermissions` delegates to
`provider(path).setAttribute(path, "posix:permissions", perms)`, throwing
`UnsupportedOperationException` only when the file system's default
provider doesn't support the POSIX attribute view (e.g. genuinely on
Windows) — never on Linux with the default file system.

## Root cause (narrowed, exact registration point not pinned down)
Confirmed via a standalone repro (no H2) that the default `FileSystem`'s
advertised attribute-view set is the problem, not just the `setAttribute`
call:
```java
Path p = Files.createTempFile("x", ".tmp");
System.out.println(p.getFileSystem().supportedFileAttributeViews());
```
- **HotSpot JDK25** (Linux): `[owner, dos, basic, posix, user, unix]`.
- **CratonVM** (`cratonvm-h2-fail-triage-20260721 --java-home
  /home/victor/jdk25`, real-JDK mode): `[]` — **empty**. `Files.setPosixFilePermissions`
  (`Files.java:1987`) checks the file system's provider for POSIX support
  before doing anything else and throws `UnsupportedOperationException`
  immediately once it finds none advertised.

So this is real-JDK-mode's default `FileSystemProvider`/`FileSystem`
reporting **zero** supported `FileAttributeView`s at all (not specifically a
POSIX gap — `dos`/`owner`/`basic`/`user`/`unix` are equally unadvertised),
even though ordinary file I/O obviously works. Not root-caused to the exact
native registration/class responsible for `supportedFileAttributeViews()`
in this session — flagged for a follow-up with more budget to trace
`sun.nio.fs.UnixFileSystemProvider`/whatever CratonVM substitutes for it in
real-JDK mode.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestFileSystem
```
or standalone:
```java
java.nio.file.Files.setPosixFilePermissions(
    java.nio.file.Path.of("/tmp/some-file"),
    java.util.EnumSet.of(java.nio.file.attribute.PosixFilePermission.OWNER_READ));
```
