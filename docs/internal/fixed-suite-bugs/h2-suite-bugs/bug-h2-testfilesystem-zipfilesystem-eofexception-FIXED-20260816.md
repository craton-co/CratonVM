# `TestFileSystem.testZipFileSystem` — `EOFException` reading a zip entry — FIXED 2026-08-16

## Status
**✅ FIXED 2026-08-16**, same day it was filed. Was
`docs/known-issues/h2/bug-h2-testfilesystem-zipfilesystem-eofexception-20260816.md`.

One line of the JDK's `ZipFile` contract: a **zero-length read at the end of a
DEFLATED entry** answers `0`, and CratonVM answered `-1`.

The page's two opening hypotheses were both wrong, and saying so is the useful
part of this record:

* It is **not** `jdk.nio.zipfs` / `jar:file:…!/…`. `testZipFileSystem` drives
  H2's own `zip:` and `zip2:` `FilePath` implementations
  (`org.h2.store.fs.zip.FilePathZip` over `java.util.zip.ZipFile`, and
  `org.h2.dev.fs.FilePathZip2`). The JDK's zip filesystem provider is not on
  this path at all.
* It is **not** the positional-I/O (`addr`/`len`/`pos`) family the closed
  `pread0` page describes. No `pread`/`pwrite` runs here; the bytes come out of
  an `InputStream` the zip shim hands back.

## The defect

`ZipFile.getInputStream(ZipEntry)` is a CratonVM native
(`native-io/src/zip_real_jar.rs`, registered for both `java/util/zip/ZipFile`
and `java/util/jar/JarFile`). It inflates the whole entry eagerly and returns a
`java.io.ByteArrayInputStream` over the result. That sidesteps a streaming
`Inflater` bridge, and for the case it was written for — reading `MANIFEST.MF`,
`../../../../apps/META-INF/services/*`, class bytes — it is indistinguishable from the real
thing.

It is not indistinguishable at end-of-entry. MEASURED, Temurin 25.0.3
(`ZeroLen2Probe`, both VMs, same probe):

| entry method | HotSpot class | `read(b,0,0)` at EOF | `available()` | `markSupported()` |
|---|---|---|---|---|
| DEFLATED | `ZipFile$ZipFileInflaterInputStream` | **0** | exact | false |
| STORED | `ZipFile$ZipFileInputStream` | **-1** | exact | false |
| — | CratonVM, both methods: `ByteArrayInputStream` | **-1** | exact | true |

The two JDK classes genuinely disagree, and for the same reason each is
internally consistent: `InflaterInputStream.read` returns 0 for `len == 0`
before it looks at anything else, while `ZipFileInputStream.read` checks
`rem == 0` first. `ByteArrayInputStream` checks `pos >= count` first, so it
matches the STORED row and misses the DEFLATED one.

`java.io.InputStream`'s own contract says a zero-length read returns 0, so a
caller written against the interface reads `-1` as end-of-file. H2's
`FileUtils.readFully(FileChannel, ByteBuffer)` is exactly that caller:

```java
do {
    int r = channel.read(dst);
    if (r < 0) { throw new EOFException(); }
} while (dst.remaining() > 0);
```

`FileZip.read` forwards straight to the entry stream, and the test's op #1
clamps its length with `len = Math.min(len, data.length - pos)` — which is
**0** whenever the cursor is already at the end of the entry. So H2 asked for
the zero remaining bytes at EOF, CratonVM said -1, and `readFully` threw.

Deterministic: the test seeds `new Random(1)` per prefix, so it is the same
sequence every run. `ZipFsProbe` (a standalone replay of the test method)
reproduced it at outer iteration 1, `pos=3459 datalen=3459` — cursor exactly at
end-of-entry — on `zip:` and `cache:zip:`, and never on `zip2:` (whose
`FilePathZip2` reads through its own channel and never touches this stream).

## The fix

`native-io/src/zip_real_jar.rs`, `native_jarfile_get_input_stream`: read the
entry's compression method alongside its bytes, and for a non-STORED entry wrap
the `ByteArrayInputStream` in a `java.io.PushbackInputStream`
(`wrap_inflater_like`).

`PushbackInputStream` is the wrapper that reproduces the DEFLATED row on every
observable this shim can be asked about: it returns 0 for `len == 0`
unconditionally, reports `markSupported()` as **false** (as both JDK zip
streams do, and unlike the bare `ByteArrayInputStream`), and leaves
`available()` exact. Handing out a real `java.util.zip.InflaterInputStream`
over the raw deflate bytes was the other candidate and is worse: its
`available()` answers 1 until EOF, where the JDK's
`ZipFileInflaterInputStream` overrides it to the exact remaining count — so
that would have traded this bug for a subtler one across every jar-reading path
in the VM.

STORED entries keep the bare `ByteArrayInputStream`, which already matches the
JDK on the value that matters (`-1`).

## Verified

MEASURED on the Azure host (`azureuser@20.80.105.49`), `--java-home
/data/toolchain/jdk-25 --nojit --Xmx 1g`, one class per scratch CWD.

`ZipFsProbe`, all four prefixes the test uses:

```
                 before          after      HotSpot
zip:             EOFException    OK         OK
cache:zip:       EOFException    OK         OK
zip2:            OK              OK         OK
cache:zip2:      OK              OK         OK
```

`ZeroLen2Probe` after the fix, DEFLATED row:
`cls=java.io.PushbackInputStream freshAvail=1000 mark=false atEOF-read(b,0,0)=0
eofAvail=0` — every column equal to HotSpot's `ZipFileInflaterInputStream`.

`org.h2.test.unit.TestFileSystem`, the class itself:

```
pristine dev : FAIL at 2.49 s
   Exception: java.io.EOFException  klength 3459 kreadFully 893 kgetFilePointer
   … kreadFully 41 kreadFully 126 kreadFully 0
                                            ^ the zero-length read at EOF
fixed        : no failure; the class does not COMPLETE, for an unrelated and
               already-documented reason (below)
HotSpot      : PASS at 7.1 s
```

The class does not finish under CratonVM within 20 minutes, before or after
this fix, and that is the pre-existing `testConcurrent` throughput wall the
retired `bug-h2-testfilesystem-testconcurrent-async-hang-FIXED` write-up
characterises (">18 minutes against HotSpot's 862 ms", and a timeout-free run
that did not complete in 3600 s). A `--stack-sample-ms 900000` capture on the
fixed binary names it exactly:

```
tid=25 name="org.h2.test.unit.TestFileSystem$1:6"
  org/h2/util/Task.run
  org/h2/test/unit/TestFileSystem$1.call
  org/h2/store/fs/niomem/FileNioMem.read
  org/h2/store/fs/niomem/FileNioMemData.readWrite
  org/h2/store/fs/niomem/FileNioMemData.addToCompressLaterCache
  org/h2/store/fs/niomem/FileNioMemData$CompressLaterCache.put / removeEldestEntry
  org/h2/store/fs/niomem/FileNioMemData.compressPage
  org/h2/compress/CompressLZF.compress
```

`testConcurrent` on the LZF in-memory filesystem, fifteen minutes after the
zip section it used to die in. Closing that is a throughput project, not this
one.

## Regression cover

`regression-suite/src/RFileTimes.java` gains a fifth stage,
`entryStreamContract`: it writes one DEFLATED and one STORED entry and emits
`available()`, a zero-length read fresh AND at EOF, a one-byte read at EOF,
`skip` forward / past the end / at the end, and the drained content, for both.
The suite diffs the whole run against HotSpot byte for byte.

```
pristine dev binary : RFileTimes FAIL  output differs from HotSpot
fixed binary        : RFileTimes PASS  (69 checks)
```

Whole suite, both arms: CORE 42 pass / 1 fail (this vector) before → **43 pass
/ 0 fail** after; JDK-only corpus 28 / 0 on both, so nothing else moved.

`markSupported()` is deliberately emitted for the DEFLATED entry only. The
STORED stand-in still answers `true` where both JDK zip streams answer `false`
— a capability offered where the JDK offers none (mark/reset genuinely work on
it), so no caller can break on it, and the wrapper that would close it has an
`InputStream`-default `skip` read-loop, a real cost on stored nested jars for
no behavioural gain. The vector says so in a comment rather than leaving the
row silently unpinned.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestFileSystem
```
`ZipFsProbe` is the faster loop: it replays one prefix of `testZipFileSystem`
with the same seeded `Random` and reports the first differing op instead of a
100-line trace.

## Related
* The retired `bug-h2-testfilesystem-pread0-bad-addr-len-pos-CLOSED-20260807`
  write-up — same test class, and explicitly NOT the same root cause; ruled out
  above rather than assumed unrelated.
* The retired `bug-h2-testfilesystem-testconcurrent-async-hang-FIXED` write-up
  — same class, throughput rather than correctness.
