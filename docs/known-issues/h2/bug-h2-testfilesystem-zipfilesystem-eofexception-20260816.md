# `TestFileSystem.testZipFileSystem` — `EOFException` reading through NIO's zip filesystem provider

## Status
**OPEN, confirmed CratonVM-specific** — found 2026-08-16 on a GC-sweep rerun
(`origin/dev` @ `f80a4b775`, Azure host `azureuser@20.80.105.49`).
Differential-verified against real HotSpot JDK 25: **HotSpot PASSes in
6.1s**, same classpath, same H2 checkout.

Distinct from the two already-closed `TestFileSystem` docs in
`docs/internal/fixed-suite-bugs/h2-suite-bugs/`:
* `bug-h2-testfilesystem-pread0-bad-addr-len-pos-CLOSED-20260807.md` — a
  *plain-disk* `pread0`/`pwrite0` IOException, closed as not-reproducible.
  Different filesystem, different exception type.
* `bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md` — retired,
  about `testConcurrent` throughput across the LZF in-memory filesystems.
  Not about zip filesystems, and explicitly marked non-normative.

Neither covers this symptom — `TestFileSystem` currently has an
**undocumented, live failure** distinct from both.

## The failure
```
Exception: java.io.EOFException
	at org/h2/test/unit/TestFileSystem.main(TestFileSystem.java:55)
	at org/h2/test/unit/TestFileSystem.test(TestFileSystem.java:68)
	at org/h2/test/unit/TestFileSystem.testZipFileSystem(TestFileSystem.java:113)
	at org/h2/test/unit/TestFileSystem.testZipFileSystem(TestFileSystem.java:198)
```
(The log line itself renders the operation trace with no separators between
entries — `klength`, `kreadFully`, `kgetFilePointer`, `kseek` — which is the
test's own compact per-call trace format, not a CratonVM logging defect;
`k` is its record delimiter. Reading it as a sequence: a `length` probe, then
several `readFully`/`getFilePointer`/`seek` calls at growing offsets, ending
in a `readFully` that returns `0` bytes where more were expected —
consistent with reading past the end of a zip entry's uncompressed data.)

`testZipFileSystem` (`TestFileSystem.java:113/158/198`) exercises H2's file
abstraction layered over the JDK's built-in `jdk.nio.zipfs.ZipFileSystem`
(`jar:file:...!/...` URIs) — positional reads (`FileChannel.read(buf, pos)`
family) through a zip entry, not the plain-disk path the already-closed
`pread0` doc covered.

## Next steps
* Isolate with a minimal repro outside H2/TestFileSystem: create a small zip
  via `java.util.zip.ZipOutputStream`, open it via
  `FileSystems.newFileSystem(URI.create("jar:file:..."), Map.of())`, and
  drive the same `readFully`/`seek`/`getFilePointer` sequence the H2 test
  does (via `SeekableByteChannel` or `RandomAccessFile`-style positional
  reads through the zipfs provider) to find the exact offset/length where
  the `EOFException` fires versus where real data actually ends.
* Given the already-closed plain-disk `pread0` doc's own finding that
  positional-I/O native argument passing (`addr`/`len`/`pos`) had a real,
  if since-unreproducible, defect class, check whether this is the SAME
  family reappearing through a different filesystem provider (zipfs delegates
  to its own internal channel implementation, not necessarily the same
  native path plain-disk I/O uses) rather than assuming they're unrelated.
* Determine whether the miscount is in zipfs's own decompression/entry-size
  bookkeeping (a CratonVM-side gap in support for `jdk.nio.zipfs`
  specifically) or a more general positional-read boundary bug that zipfs
  merely exposes first.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestFileSystem
```
Fails fast (well under the 900s cap, unlike the retired throughput-wall doc's
multi-minute `testConcurrent` symptom).

## Related
* [`bug-h2-testfilesystem-pread0-bad-addr-len-pos-CLOSED-20260807.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-pread0-bad-addr-len-pos-CLOSED-20260807.md) — same class, different filesystem and exception type; worth ruling out a shared positional-I/O root cause rather than assuming unrelated.
