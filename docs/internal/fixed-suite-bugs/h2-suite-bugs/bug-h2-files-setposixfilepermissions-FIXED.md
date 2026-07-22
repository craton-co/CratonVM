# `java.nio.file.Files.setPosixFilePermissions` throws `UnsupportedOperationException` under real-JDK mode on Linux

## Status
**FIXED** — 2026-07-22, `dev` (see commit referenced in the merge that landed this
doc move). Originally opened 2026-07-21 as
`docs/known-issues/h2-suite-bugs/bug-h2-files-setposixfilepermissions-unsupported.md`.

## Severity (as filed)
**MEDIUM** — any code that sets POSIX file permissions via the standard
`java.nio.file` API (rather than legacy `File.setReadOnly()`/`chmod`
shelling out) fails on a platform (Linux) where this should always be
supported.

## Affected test classes
- `org.h2.test.unit.TestFileSystem` (`testSetReadOnly`) — the specific
  `setReadOnly()`/`setPosixFilePermissions` path is fixed; the class as a
  whole is **still not a clean PASS** because of an unrelated, pre-existing
  bug further down in the same class — see "Residual not fixed here" below.
- `org.h2.test.unit.TestTraceSystem` (`testReadOnly`) — **fully fixed**,
  confirmed by rerunning the real class end-to-end (see Verification).

## Original symptom
```
java.lang.UnsupportedOperationException
	at java/nio/file/Files.setPosixFilePermissions(Files.java:1987)
	at org/h2/store/fs/disk/FilePathDisk.setReadOnly(FilePathDisk.java:291)
	at org/h2/store/fs/FileUtils.setReadOnly(FileUtils.java:330)
```

## Root cause (fully pinned down)
The original doc narrowed this to "the default `FileSystem` advertises zero
`FileAttributeView`s" but didn't pin the exact native gap. Tracing
`FilePathDisk.setReadOnly()`'s real body showed it actually does:
```java
FileStore fileStore = Files.getFileStore(f);
if (fileStore.supportsFileAttributeView(PosixFileAttributeView.class)) {   // always true (native stub)
    HashSet<PosixFilePermission> permissions = new HashSet<>();
    for (PosixFilePermission p : Files.getPosixFilePermissions(f)) { ... } // succeeds silently (see below)
    Files.setPosixFilePermissions(f, permissions);                        // <-- this is what throws
}
```
`Files.getPosixFilePermissions(f)` succeeds because it happens to resolve
(via `FileSystemProvider.readAttributes(Path, Class, LinkOption[])`) to a
*real* `sun/nio/fs/UnixFileAttributes` object built by
`p59_files_read_attributes`/`basic_file_attributes_alloc` — a real JDK class
implementing `PosixFileAttributes`, so its own real `permissions()` bytecode
works once `st_mode` carries real bits (see fix #3 below). The actual crash
is in `Files.setPosixFilePermissions`, whose real bytecode is:
```java
PosixFileAttributeView view = getFileAttributeView(path, PosixFileAttributeView.class);
if (view == null) throw new UnsupportedOperationException();
view.setPermissions(perms);
```
`FileSystemProvider.getFileAttributeView(Path, Class, LinkOption[])`
(`native-builtins/src/phases_late.rs`) only ever handled
`BasicFileAttributeView`/`DosFileAttributeView` — a request for
`PosixFileAttributeView` fell through to `null`, and real bytecode turns
that into the `UnsupportedOperationException` above. `FileSystem.
supportedFileAttributeViews()` was also an unconditional `EmptySet` (matches
the original doc's finding), which is the visible symptom callers that
pre-check `contains("posix")` would hit.

## Fix (native-builtins/src/phases_late.rs)
1. **`FileSystemProvider.getFileAttributeView`**: added a `PosixFileAttributeView`
   branch (real on Linux/macOS, still null on Windows — matches HotSpot).
   Returns a synthetic view (Path in field 0), same shape as the existing
   Basic/Dos handling.
2. **New `PosixFileAttributeView` methods**: `readAttributes()` (delegates
   to the existing `p59_files_read_attributes`, which already produces a
   real `sun/nio/fs/UnixFileAttributes` on Linux), `name()` → `"posix"`,
   `setPermissions(Set)` (chmods the real backing file via
   `std::fs::set_permissions`/`PermissionsExt`, converting the
   `Set<PosixFilePermission>` via a new `posix_permission_bits_from_set`
   helper), `setTimes` (added to the existing Basic/Dos loop), and
   `getOwner`/`setOwner` (no-ops, matching `Files.getOwner`'s existing
   unsupported-owner-lookup behavior — not exercised by these tests).
3. **`basic_file_attributes_store`**: was hardcoding `st_mode` to the bare
   file-type bits with **zero permission bits**, so
   `UnixFileAttributes.permissions()` always answered an empty set
   regardless of the file's real mode — harmless until `PosixFileAttributeView`
   became reachable, at which point `FilePathDisk.setReadOnly`'s "keep
   everything except `*_WRITE`" recomputation had nothing to keep. Now ORs
   in the real mode bits (`meta.permissions().mode() & 0o7777` on unix) for
   the real-host-path case.
4. **`FileSystem.supportedFileAttributeViews()`**: was an unconditional
   `EmptySet`; now reports `[owner, dos, basic, posix, user, unix]` on
   Linux (`[basic, dos, acl, owner, user]` on Windows) — matches HotSpot
   exactly for a real (non mounted-jar/jrt) filesystem, verified via
   `p.getFileSystem().supportedFileAttributeViews()`. Virtual (jar/jrt)
   filesystems keep reporting empty (unchanged, out of scope here).
   **Adjacent bug caught while implementing this**: the first attempt used
   the existing `build_string_set` helper (naive array/size/capacity
   synthetic `HashSet` layout) — that layout silently prints/iterates as
   empty under real-JDK mode, because real `AbstractCollection.toString()`/
   `HashSet.iterator()` bytecode reads the *real* `HashSet.map` field
   expecting a real `HashMap`, not a raw backing array. Switched to the
   already-established `build_real_layout_string_hashset` helper (same
   fix pattern used elsewhere in this file for exactly this class of bug),
   which builds a genuinely real `HashMap`+`HashSet$Node` chain.

`Files.isWritable(Path)` was also audited (H2's `FilePathDisk.canWrite()`
calls it directly, so `testSetReadOnly`'s `assertFalse(canWrite(...))`
depends on it reflecting real permission bits) — there are two competing
native registrations on `java/nio/file/Files` for this method (phase57 and
phase61); phase61's (registered later, so it wins in the shared
last-write-wins native registry) already correctly checked
`!metadata.permissions().readonly()`. The phase57 copy (existing-but-shadowed,
existence-only) was fixed to match anyway, since a registration-order change
would otherwise silently resurrect the bug.

## Residual NOT fixed here (new, separate finding)
Rerunning the real `org.h2.test.unit.TestFileSystem` end-to-end (not just the
`testSetReadOnly` sub-test) shows the class still fails — but now at
`testSimple()` (`TestFileSystem.java:495`), unrelated to POSIX permissions:
```
AssertionError: Expected an exception of type NonWritableChannelException to
be thrown, but the method returned sun.nio.ch.FileChannelImpl@...
```
`FileUtils.open(path, "r")` (read-only mode) returns a `FileChannel` that
still permits `.write()`/`.truncate()` — CratonVM's `FileChannel.open`/mode
dispatch doesn't enforce the read-only open mode. This is a distinct bug in
file-channel open-mode handling, not in POSIX attribute views; it does not
block `testSetReadOnly` (which runs and passes earlier in the same class,
confirmed by execution reaching the later, unrelated failure). Flagged as a
follow-up, not fixed in this session.

## Verification
- Standalone repro (`Files.createTempFile` → read/clear/restore permissions
  via the real `java.nio.file.attribute` API) run 4× against the fixed
  binary: `supportedFileAttributeViews`, before/after permission strings,
  and `isWritable` all match the HotSpot JDK25 baseline exactly, no
  exceptions, no flakiness.
- `org.h2.test.unit.TestTraceSystem` (real class, full run): **PASS** (was
  the UOE crash above).
- `org.h2.test.unit.TestFileSystem` (real class, full run): `testSetReadOnly`
  confirmed passing (execution progresses to the later, unrelated
  `testSimple` failure described above — proof the fixed code path actually
  ran and succeeded, not just a synthetic probe).
- `org.h2.test.unit.TestFile` (real class, full run): PASS, no regression.
- Built with `CARGO_PROFILE_RELEASE_LTO=off` on the Azure Linux host in an
  isolated worktree/branch (`fix/h2-posix-permissions-20260722`), not the
  shared main checkout.
