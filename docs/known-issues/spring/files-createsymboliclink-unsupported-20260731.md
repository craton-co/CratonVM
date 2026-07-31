# `Files.createSymbolicLink` always throws `UnsupportedOperationException` — no real symlink creation

**Status: OPEN — found 2026-07-31**

## Symptom

`org.springframework.boot.env.ConfigTreePropertySourceTests` — 3/23 failures, all identical shape:

```
java.lang.UnsupportedOperationException
   java.nio.file.spi.FileSystemProvider.createSymbolicLink(FileSystemProvider.java:626)
   java.nio.file.Files.createSymbolicLink(Files.java:976)
   org.springframework.boot.env.ConfigTreePropertySourceTests.createSymbolicLink(ConfigTreePropertySourceTests.java:302)
```

Affects `getPropertyNamesFromNestedWithSymlinkInPathReturnsPropertyNames`,
`getPropertyNamesFromFlatWithSymlinksIgnoresHiddenFiles`,
`getPropertyNamesFromNestedWithSymlinksIgnoresHiddenFiles`.

`org.springframework.boot.system.ApplicationTempTests` shows the same underlying cause but is not
actually a JUnit failure: `whenSymlinkExistsInDirectoryLocationGetDirThrows` wraps
`Files.createSymbolicLink` in a `try/catch` and calls `Assumptions.abort("Symlink creation not
supported")` on any exception (`ApplicationTempTests.java:106-120`) — exactly the graceful
degradation path this test was written for. The suite run shows `tests=5 failed=0 aborted=1
skipped=1`, i.e. 0 real failures; the suite runner's `results.tsv` nonetheless marks the class row
`status=FAIL` purely because `aborted>0`. That classification is a suite-runner artifact, not a
test failure — but it shares the same root cause below, so it's covered by this doc rather than
filed separately.

## Root cause

`Files.createSymbolicLink(Path, Path, FileAttribute...)` is real (non-native) Spring/JDK bytecode;
it delegates to `link.getFileSystem().provider().createSymbolicLink(...)`. CratonVM's default
`FileSystemProvider` instance is a synthetic object literally stamped as the abstract
`java/nio/file/spi/FileSystemProvider` class itself (see `native-builtins/src/phases_late/nio_file.rs:1677-1691`,
which documents the same pattern for `getFileStore`): every method the synthetic provider needs
must be individually registered as a native override on the `java/nio/file/spi/FileSystemProvider`
class (`fsp` in that file), or the call lands on the base class's own method body.

`createSymbolicLink`/`createLink`/`readSymbolicLink` are **not** among the natives registered on
`fsp`. There *is* a native stub registered for these three names
(`native-builtins/src/phases_late/nio_file.rs:16139-16153`) — but it's registered against
`java/nio/file/Files` (the static wrapper class), not `java/nio/file/spi/FileSystemProvider` (the
object whose method is actually invoked by `Files.createSymbolicLink`'s bytecode). Since
`Files.createSymbolicLink` is not itself a native method, that registration is never reached — it
is dead code. The call instead falls through to `FileSystemProvider.createSymbolicLink`'s own
(non-abstract in CratonVM's copy) method body, which throws `UnsupportedOperationException`
unconditionally — matching the stack trace above exactly.

Note `Files.isSymbolicLink`/`readSymbolicLink` for *existing* links partially work (registered
separately, further up the same file, at `nio_file.rs:4403-4420`, backed by real
`std::path::Path::is_symlink()` / a JRT-image-specific reader) — only **creation** of a new
symlink is unimplemented.

## Affected classes
- `core/spring-boot` — `org.springframework.boot.env.ConfigTreePropertySourceTests`
- `core/spring-boot` — `org.springframework.boot.system.ApplicationTempTests` (no real JUnit
  failures; FAIL status is a suite-runner aborted-count artifact, same root cause)
- `core/spring-boot-autoconfigure` — `org.springframework.boot.autoconfigure.ssl.FileWatcherTests`
  (confirmed 2026-07-31 rerun): 5 of its 14 failures are the identical
  `UnsupportedOperationException` at `FileSystemProvider.createSymbolicLink(FileSystemProvider.java:626)`
  → `Files.createSymbolicLink(Files.java:976)`, from `shouldTriggerOnConfigMapUpdates`,
  `shouldFollowSymlinkRecursively`, `shouldFollowRelativePathSymlinks`,
  `shouldTriggerOnConfigMapAtomicMoveUpdates`, `shouldFollowSymlink`. The remaining 9 failures in
  this class are a separate, unrelated bug — see
  `filewatcher-watchservice-timed-poll-missing-native-20260731.md`.
