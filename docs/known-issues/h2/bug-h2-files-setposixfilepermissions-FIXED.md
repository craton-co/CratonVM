# `java.nio.file.Files.setPosixFilePermissions` throws `UnsupportedOperationException` under real-JDK mode on Linux

## Status
**FIXED, PLUS FIVE MORE RESIDUAL BUGS FOUND AND FIXED IN THE SAME CHAIN — ONE
NEW, UNRELATED RESIDUAL REMAINS OPEN.** The POSIX-permissions bug this doc
originally covers was fixed 2026-07-22 on `dev`. A 2026-07-23 follow-up
session set out to close the `TestFileSystem` residual chain and, by fixing
each failure in turn, found and fixed five more independent real bugs the
class was hiding behind the first one (see "Residual chain fixed
2026-07-23" below). After all six fixes, `TestFileSystem` progresses much
further than before but still does not reach a clean PASS — it now hangs in
`testConcurrent` against the `async:` filesystem, a genuinely separate,
deeper JIT/threading bug tracked in its own new doc,
[`bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md).
Kept in `docs/known-issues/` per this repo's convention (a doc stays open as
long as any affected test class has an open sub-item — `TestFileSystem`
still does, just for a different reason now). Originally opened 2026-07-21 as
`/bug-h2-files-setposixfilepermissions-unsupported.md`.

## Severity (as filed)
**MEDIUM** — any code that sets POSIX file permissions via the standard
`java.nio.file` API (rather than legacy `File.setReadOnly()`/`chmod`
shelling out) fails on a platform (Linux) where this should always be
supported.

## Affected test classes
- `org.h2.test.unit.TestFileSystem` (`testSetReadOnly`) — the specific
  `setReadOnly()`/`setPosixFilePermissions` path is fixed. The class as a
  whole is **still not a clean PASS**, but the blocker is now
  `testConcurrent`'s `async:`-filesystem hang (separate doc, see Status
  above) — every other sub-test, including the read-only-`FileChannel`
  residual originally flagged here, is now fixed (see below).
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

## Residual chain fixed 2026-07-23

The original doc's own residual (`FileChannel.write()`/`truncate()` not
enforcing read-only open mode) turned out to be the first of a chain of six
independent bugs, each blocking `TestFileSystem` at a different point. Fixed
in order, in `fix/h2-filechannel-readonly-mode-20260723`:

1. **`sun/nio/ch/FileChannelImpl.truncate` didn't check `writable`.** A
   Kafka-motivated native override registered directly on the *concrete*
   class `sun/nio/ch/FileChannelImpl` (bug-27, `native-builtins/src/lib.rs`)
   completely bypasses that method's real bytecode — including the
   `writable` field check real bytecode does before throwing
   `NonWritableChannelException`. `write()` had no such concrete-class
   override, so it already worked correctly via real bytecode; only
   `truncate()` needed the check added. This is the original symptom:
   `FileUtils.open(path, "r")` then `.write()`/`.truncate()` silently
   "succeeded" instead of throwing.
2. **Same method didn't respect "no-op when new size >= current size".**
   The bug-27 override unconditionally called `rw_set_length(fd, new_len)`,
   which *grows* (zero-extends) the file when `truncate()` is called with a
   size larger than the current one — real `FileChannel.truncate()` is a
   no-op in that case. Caught by `testRandomAccess`, which runs the same op
   sequence against a real `RandomAccessFile` oracle.
3. **`Files.move(Path, Path, CopyOption...)` never checked
   `REPLACE_EXISTING`.** It called `std::fs::rename` directly — unconditional
   POSIX `rename(2)` semantics — so a target that already existed was
   silently overwritten instead of throwing `FileAlreadyExistsException`
   (H2's `FilePathDisk.moveTo(newName, false)` catches that and converts it
   to `DbException`). Caught by `testMoveTo`.
4. **`AsynchronousFileChannel.read`/`write` (native-io) returned a bare
   unboxed `Value::Int` inside their completed `Future`.**
   `Future<Integer>.get()`'s real bytecode does `checkcast Integer` on the
   result — a bare int crashed the whole VM ("internal error: checkcast:
   not an object reference") the first time `testConcurrent` exercised the
   `async:` filesystem. Fixed by boxing via the existing (but, on this path,
   unused) `afc_box_integer` helper, matching the sibling
   `CompletionHandler`-based overloads that already did this correctly.
5. **`AsynchronousFileChannel.tryLock(long, long, boolean)` was completely
   unregistered** (`AbstractMethodError: method ... has no Code attribute`
   — it really is abstract in the JDK, with no fallback bytecode path).
   Fixed by building a real `sun/nio/ch/FileLockImpl` via its
   `(AsynchronousFileChannel, long, long, boolean)` constructor. That
   exposed a second gap: `FileLockImpl.release()`'s real bytecode does
   `instanceof FileChannelImpl` / `instanceof AsynchronousFileChannelImpl`
   to decide how to update the JDK's internal per-file lock table — our
   synthetic (literal-class-named) `AsynchronousFileChannel` matches
   neither, hitting the bytecode's `AssertionError` fallback branch. Fixed
   with a class-level override on `FileLockImpl.release()` that replicates
   the real control flow exactly (so real `FileChannel`-based locks, which
   worked correctly before via pure real bytecode, are unaffected) and only
   substitutes behavior for the synthetic-channel case.
6. **`AsynchronousFileChannel.write`/`truncate` didn't check writability**,
   throwing a generic `IOException` ("channel was not opened for writing")
   instead of `NonWritableChannelException` — the same contract gap as #1,
   just in the async implementation. Fixed the same way.

Each fix was verified independently: after each one, `TestFileSystem`'s
failure moved forward to a *new*, previously-unreached assertion or
exception — never regressed to an earlier failure — confirming the fixes
compose correctly rather than papering over each other.

**Full 218-class H2 suite regression check** (same binary, `--jit on`,
`--class-to 90`, Azure Linux host) after all six fixes: **PASS 144 / HANG 53
/ FAIL 20 / CRASH 1**, versus the same-day pre-fix baseline (`RESULTS-20260723.md`,
before this session's fixes landed) of **PASS 139 / HANG 60 / FAIL 19 /
CRASH 0** — a net improvement (+5 PASS, −7 HANG). The one new `FAIL`
(`org.h2.test.unit.TestFileLock`) and the one `CRASH`
(`org.h2.test.unit.TestTimeStampWithTimeZone`) were both inspected and are
unrelated to this chain: `TestFileLock` exercises H2's own hand-rolled
`org.h2.store.FileLock` lock-file protocol (nothing to do with
`java.nio.channels.FileLock`), and `TestTimeStampWithTimeZone` panics inside
`java/time/ZoneRegion.ofId` value-stack indexing, unrelated to any file/NIO
code touched here.

## New residual: `testConcurrent` hangs against the `async:` filesystem
With the above six fixes in place, `TestFileSystem` reaches `testConcurrent`
for the first time (previously blocked earlier in the class) and hangs
there running against the `async:` filesystem prefix. Live-gdb evidence
narrows this to the JIT's on-stack-replacement (`CompilationPolicy` mutex)
subsystem interacting with a `LinkedHashMap.put()` eviction callback under
real multi-thread contention — a genuinely separate, deeper bug, **not**
part of this doc's scope. Tracked in its own doc:
[`bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md).

## Verification
- Standalone repro (`Files.createTempFile` → read/clear/restore permissions
  via the real `java.nio.file.attribute` API) run 4× against the fixed
  binary: `supportedFileAttributeViews`, before/after permission strings,
  and `isWritable` all match the HotSpot JDK25 baseline exactly, no
  exceptions, no flakiness.
- `org.h2.test.unit.TestTraceSystem` (real class, full run): **PASS** (was
  the UOE crash above).
- `org.h2.test.unit.TestFileSystem` (real class, full run): `testSetReadOnly`,
  `testSimple`, `testRandomAccess`, `testMoveTo`, `testDirectories`, and
  `testTempFile` all confirmed passing after the 2026-07-23 residual-chain
  fixes (execution now progresses all the way to `testConcurrent`, the new
  residual above).
- `org.h2.test.unit.TestFile` (real class, full run): PASS, no regression.
- Standalone `FileChannel`/`Files.move`/`AsynchronousFileChannel` repro
  programs (read-only write/truncate, move-onto-existing-file,
  `AsynchronousFileChannel.open`+`write`+`tryLock`+`release`) all confirmed
  matching real HotSpot JDK25 behavior.
- Full 218-class H2 suite regression sweep (see above): net improvement,
  no regression attributable to these fixes.
- Built with `CARGO_PROFILE_RELEASE_LTO=off` on the Azure Linux host in
  isolated worktrees/branches (`fix/h2-posix-permissions-20260722` for the
  original fix, `fix/h2-filechannel-readonly-mode-20260723` for the
  residual chain), not the shared main checkout.
