# L4 — `java.io` and `java.nio`: ~335 rows

> **RETIRED 2026-08-28 — the lane is done and this brief is history.**
>
> Read the record instead:
> `known-issues/jdk-only/L4-the-io-and-nio-worklist-49-defects-and-a-bounds-check-that-killed-the-vm-20260828.md`.
>
> **What the lane did.** The `native-won` surface was mined rather than guessed:
> **199 distinct triples** across `java/io` and `java/nio`, from a
> `--jdk-only-report` over `probes/L4Reach.java`. Five new differential probes
> cover all of them — `L4FileSweep` (331 rows), `L4FilesSweep` (395),
> `L4ByteBufferSweep` (404), `L4PrintStreamSweep` (123), `L4StreamTailSweep`
> (208) — **1461 rows against HotSpot 25.0.4+7, in both modes.**
>
> **49 defects, all fixed.** Including a `PrintStream.write(byte[], 0, -1)` that
> **panicked the VM** (`capacity overflow` — `-1 as usize`), a
> `sun.nio.ch.FileChannelImpl.truncate(-1)` that clamped to zero and DELETED the
> file, `Files.copy`/`move` raising `IllegalStateException` (not an
> `IOException` at all, so `catch (IOException)` could not see it),
> `Path.startsWith` implemented as a STRING prefix (so `/appsecret` starts with
> `/app`), a `SimpleFileVisitor` whose `visitFileFailed` answered `CONTINUE` and
> therefore swallowed every walk error, and a `PrintStream` that ignored its own
> charset on every text write.
>
> **Final state: 1458 of 1461 rows identical in both modes.** The three
> residuals are recorded with their measurements in the record's §4 — one is
> contract-equivalent (`newBufferedReader` on a directory throws the same type,
> earlier), one is not a contract at all (`BufferedWriter` does not buffer), and
> one is a RESOLUTION finding no registrar edit can move
> (`FileInputStream.skip` resolves to its superclass; both candidate natives
> report `invocations: 0`).
>
> **Four nominations for other lanes** are in the record's §4: retire the two
> `Files.newBufferedReader` registrations (they read the whole file at open);
> retire the six `java/io/BufferedWriter` registrations (their fd-backed arm is
> `#[cfg(feature = "synthetic-jdk")]` and dead in every shipping build, so both
> shipping modes run pure pass-throughs); the `FileInputStream.skip` resolution
> defect; and `p57_alloc_provider` minting the default `FileSystemProvider` as
> an instance of the ABSTRACT `java/nio/file/spi/FileSystemProvider` — the same
> fabricated-abstract-receiver shape as the roadmap's Phase-1 `MemorySegment`
> row, and the cause of the `NoSuchMethodError` that `Files.probeContentType`
> died with.
>
> Everything below this banner is the ORIGINAL BRIEF as written on 2026-08-28,
> kept verbatim. Two of its statements did not survive contact and are corrected
> in the record: the "residual 14 recorded as OPEN" for `java.io.File` could not
> be located in any record and the family is now 0-diff over 331 rows; and the
> Windows path predicates it warns about are not reachable from the Linux build
> host, so they are covered by `nio_file.rs`'s 23 `#[cfg(windows)]` tests rather
> than by this lane's probes.

---

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
