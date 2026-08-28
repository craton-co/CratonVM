# L4 — `java.io` and `java.nio`: ~335 rows

**Read `HANDOFF-20260828-SCOPE.md` first.**

**Owner: unclaimed.** L5 (`claude/jdk-only-mode-handoff-09b48c`, worktree
`h2-known-issues-206dee`) is the only lane currently running.

## Your families

```text
java/io/File             74 bridge-with-code rows
java/nio/file/Files      59
java/nio/ByteBuffer      33
java/io/PrintStream      29
+ java/io tail (207 total in package)
+ java/nio tail (128 total in package)
                        ~335   (15%)
```

Registrars: `native-io/src/lib.rs` (31 240 lines) and
`native-builtins/src/phases_late/nio_file.rs`. Shared with L3/L6 only through
`native-collections`, which you should rarely need.

## Already done — do NOT redo

* `ByteArrayOutputStream` — 6 native-won triples probed
  (`probes/BaosCollectionsShadowSweep.java`), 2 null defects fixed. **Every
  bounds row already passed**, including `off + len` overflowing to a negative
  int. The bounds logic is right; it was not being reached.
* Windows path handling in `nio_file.rs` was rewritten earlier this campaign —
  separator-run collapsing, UNC prefix preservation, the root test,
  `file_is_absolute`, and `file_join_parent_child_units`' default-parent and
  root-parent rules, with 23 `#[cfg(windows)]` regression tests.
* `Path.relativize` now refuses a foreign root; `BufferedWriter.close` nulls
  `out`.
* `PrintStream.charset()` validates and repairs an already-set abstract charset.

## The trap this lane has already sprung twice

**`java.io.File` has a residual 14 recorded as OPEN** from an earlier sweep, and
the Windows path work above was needed because the first `relativize` fix
compared *absoluteness* — on Windows a driveless-rooted `\x` has a root and is
NOT absolute, so the guard never fired for the very row that found the defect.

Path predicates on this host are not the ones the Unix-shaped intuition
suggests. Ask each of: a drive-absolute path, a driveless-rooted path, a UNC
path, a relative path, an empty path, and a path with a trailing separator.

## Edges that pay in this lane

* **`File`**: `getParent` at a root and of a bare name (null vs a value),
  `getCanonicalPath` vs `getAbsolutePath` on a non-existent file, `list`/
  `listFiles` on a file rather than a directory (null, not empty),
  `renameTo` across roots, `length` and `lastModified` on a missing file (0),
  `delete` on a non-empty directory (false, not an exception).
* **`Files`**: `newInputStream` on a directory, `readAllBytes` on a missing file
  (`NoSuchFileException`, not `FileNotFoundException`), `createDirectories` on an
  existing directory (succeeds) vs `createDirectory` (throws), `copy` with and
  without `REPLACE_EXISTING`, `walk` depth limits, and every `Path` argument
  given `null`.
* **`ByteBuffer`**: `position`/`limit`/`mark` invariants and the exact exception
  when they are violated (`IllegalArgumentException` vs
  `InvalidMarkException`), `slice`/`duplicate` sharing, `flip`/`rewind`/`clear`,
  a read-only buffer refusing every mutator, `array()` on a direct buffer
  (`UnsupportedOperationException`), and absolute vs relative get/put bounds.
  Note `buffer-address-is-nonzero-for-heap-buffers-on-jdk21plus` before asserting
  anything about `address`.
* **`PrintStream`**: it swallows `IOException` and sets an error flag — so
  `checkError()` is the observable, and a stream that throws where the JDK
  swallows is a defect in the other direction from most of this campaign.
