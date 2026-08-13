# `TestFileSystem` dies in ~1 s: `IOException: pread0/pwrite0: bad addr/len/pos` — CLOSED 2026-08-07

**Status: ✅ CLOSED — not reproducible, negative control included.** Filed
2026-08-05 as a regression blocking `org.h2.test.unit.TestFileSystem`. It does
not fire on `dev` today, and it does not fire at the commit that filed it
either, so this page cannot claim a fix — what it can do is say exactly what was
measured, and land the diagnostic the original page asked for so the next
sighting explains itself.

## What was reported

```
java.lang.AssertionError: Exception: java.io.IOException: pread0: bad addr/len/pos
        at org.h2.test.unit.TestFileSystem.testConcurrent(TestFileSystem.java:791)
        at org.h2.test.unit.TestFileSystem.testFileSystem(TestFileSystem.java:374)
```

`testFileSystem(getBaseDir() + "/fs")` — the *plain disk* filesystem, the first
one the class exercises. The class was said to die in 0.5–1.3 s, making the
whole of it unreachable. Both `pread0` and `pwrite0` produced it; which one
surfaced varied with timing, because `testConcurrent` runs a reader thread
against a writer.

`pread0` / `pwrite0` are the positional-I/O natives behind
`FileChannel.read(ByteBuffer, long)` / `write(ByteBuffer, long)`, so if it is
real the blast radius is any positional channel I/O, not just H2's harness.

## What was measured, 2026-08-07 (Azure Linux, JDK 25 real-JDK mode)

`apps/h2database-suite-runner/probes/TfsProbe.java` runs
`TestFileSystem.testFileSystem(String)` for one prefix per argument, so the
plain-disk body — `testConcurrent` included — is about a second, not a class run.

| binary | plain-disk `testFileSystem` |
|---|---|
| `dev` @ `b21d782c9` (today) | **30/30 clean**, ~1.1 s each |
| `dev` @ `6c9905f5b` — *the commit that filed this page* | **5/5 clean**, ~1.1 s each |

The second row is the negative control, and it is the reason this page is closed
as *not reproducible* rather than as *fixed*: the arm that is supposed to fail
does not. Running the real class rather than the probe agrees — at `6c9905f5b`
the log contains **zero** exceptions of any kind before the 90 s cap.

The full twelve-prefix sequence is clean end to end on `dev` today.

## The diagnostic the original page asked for is landed

Its next step was *"print the rejected `(addr, len, pos)` triple at the refusal
site — the three inputs distinguish 'the buffer address is wrong' from 'the
length or position is wrong'. The message names all three but does not currently
include their values."*

`native-io/src/nio_native.rs` now does that, for all four of
`read0` / `pread0` / `write0` / `pwrite0`:

```
pread0: bad addr/len/pos (addr=0x0, len=16, pos=6291456) args=[L:obj, J:0, I:16, J:6291456]
```

The raw argument list is there for a reason: `long_arg` / `int_arg` answer `0`
for an argument that is absent or of the wrong `Value` shape, so a refused triple
of zeroes is ambiguous — the JDK may genuinely have passed a zero, or the
dispatch may have handed the native a shape those accessors do not read.
Printing the arguments as received separates the two without a rebuild.

## What is NOT established

* **Which change fixed it**, if a change did. It was gone before this work
  started and was never bisected.
* **Whether the original measurement was on Linux.** The original page names no
  platform, and `fd_from_descriptor` reads `handle` (Windows) before `fd`
  (Unix), so "Windows-only" was the obvious hypothesis. It was tested and is
  **not** it: a Windows release build of this same branch clears
  `testConcurrent` on the plain-disk filesystem with no refusal of any kind, on
  every prefix tried (plain disk, `nioMapped:`, `split:nioMapped:`).
* **Whether the H2 checkout matters.** The measurements here used the H2 tree at
  `/data/data/h2database/h2` on the Azure host; the original page does not name
  the checkout it used.

## What Windows does instead

The Windows arm dies earlier in the same `testFileSystem(String)` body, at
`testSetReadOnly`, with

```
java.lang.AbstractMethodError: method java/nio/file/spi/FileSystemProvider.setAttribute(...)
    has no Code attribute
```

— 3/3, on all three prefixes tried. That is a separate defect with nothing to do
with positional I/O.

**Update 2026-08-07:** it was **not** `Files.setAttribute` reaching the abstract
declaration *instead of* an override, and it was **not** Windows-only. The
default provider object is stamped with the abstract class and
`setAttribute` had no implementation registered at all, on any platform; H2 only
reaches it on Windows because `FilePathDisk.setReadOnly` takes the
`Files.setPosixFilePermissions` branch on Linux. Fixed the same day — see
[`bug-h2-files-setattribute-abstract-provider-FIXED-20260807.md`](bug-h2-files-setattribute-abstract-provider-FIXED-20260807.md)
and its follow-on
[`bug-nio-filesystemprovider-abstract-surface-residuals-FIXED-20260807.md`](bug-nio-filesystemprovider-abstract-surface-residuals-FIXED-20260807.md).
Until that fix, Windows `TestFileSystem` could not reach `testConcurrent`'s later
iterations at all, so a Windows report of the `pread0` refusal would have to
predate that blocker.

## Reproducing (if it returns)

```bash
H2=<h2 checkout>/h2
javac -cp $H2/target/classes:$H2/target/test-classes -d /tmp/tfs \
  apps/h2database-suite-runner/probes/TfsProbe.java
<binary> --java-home <jdk25> --Xmx 1g \
  -c $H2/target/classes:$H2/target/test-classes:/tmp/tfs \
  TfsProbe @BASE@/fs
```

`@BASE@` expands to `TestBase.getBaseDir()`. Run it from a scratch directory —
H2's `BASE_TEST_DIR` is `./data`, relative to the working directory. Repeat it;
one pass of a timing-dependent failure is not evidence, in either direction.

## Context

The retired `h2-jitban-longtail1` write-up records that on 2026-08-02 the class
got as far as `nioMapped:` once the `nioMemLZF:` throughput gap was closed, and
handed off a `nioMapped:` blocker. That one was real, reproduces in 12 seconds,
was root-caused to the conservative JIT root scan marking the whole native stack
on the residue of a returned compiled frame, and is fixed — see the retired
`bug-h2-niomapped-unmap-gc-timeout` write-up.
