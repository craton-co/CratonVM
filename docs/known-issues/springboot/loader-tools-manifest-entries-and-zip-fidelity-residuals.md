# `spring-boot-loader-tools` residual FAILs: manifest per-entry attributes and zip byte-fidelity

**Status: OPEN — found 2026-07-17**

Three more `loader/spring-boot-loader-tools` FAILs, likely 2 unrelated
mechanisms (the module's other FAILs —
`ImagePackagerTests.springBootVersion`/`RepackagerTests.springBootVersion`/
`jarIsOnlyRepackagedOnce` — are covered separately in
`loader-tools-spring-boot-version-manifest-attribute-missing.md`). All
confirmed CratonVM-specific (pass on the same-scope HotSpot baseline).

## Cluster 1 — signed-jar per-entry manifest attributes (digest entries) lost

| Class | Method | Wall time |
|---|---|---:|
| `FileUtilsTests` | `isSignedJarFileWhenSignedReturnsTrue()` | 0.36s (1/6 fail) |
| `RepackagerTests` | `signedJar()` | (part of the 49.7s / 9-failure run) |

```
JUnit Jupiter:FileUtilsTests:isSignedJarFileWhenSignedReturnsTrue()
  => org.opentest4j.AssertionFailedError:
Expecting value to be true but was false
       org.springframework.boot.loader.tools.FileUtilsTests.isSignedJarFileWhenSignedReturnsTrue(FileUtilsTests.java:102)

JUnit Jupiter:RepackagerTests:signedJar()
  => org.opentest4j.AssertionFailedError:
Expecting value to be true but was false
       org.springframework.boot.loader.tools.RepackagerTests.signedJar(RepackagerTests.java:216)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader-tools.org.springframework.boot.loader.tools.FileUtilsTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader-tools.org.springframework.boot.loader.tools.RepackagerTests.out.log`

`FileUtils.isSignedJarFile(File)`
(`apps/spring-boot/loader/spring-boot-loader-tools/src/main/java/org/springframework/boot/loader/tools/FileUtils.java:70-84`)
opens the jar with a real `java.util.jar.JarFile`, reads
`jarFile.getManifest()`, and checks whether **any per-entry section**
(`manifest.getEntries()`, i.e. the `Name: foo.class` sub-blocks that follow
the main attributes, one per signed file) has an attribute whose name ends
in `-Digest`. The test writes a manifest built from a real
`signed-manifest.mf` test resource (which does have such per-entry digest
sections) into a jar, then reads it back — and `hasDigestEntry` returns
`false`, meaning `getEntries()` came back empty (or without any
`*-Digest`-named attribute) after the round-trip.

`RepackagerTests.signedJar()` exercises a related but distinct path:
`Packager` copies a `META-INF/BOOT.SF` (or renamed equivalent) signature
file into the output jar when packaging a library whose own manifest has
digest `getEntries()` — `assertThat(hasPackagedEntry("META-INF/BOOT.SF")).isTrue()`
fails, meaning `Packager`'s signed-jar detection (which also goes through
manifest per-entry data) didn't recognize the library as signed either.

**Root cause: not confirmed.** Both failures point at the same underlying
gap: `java.util.jar.Manifest`'s **per-entry sections** (as opposed to its
main attributes, which work fine elsewhere in this same test run — e.g.
`Main-Class`/`Start-Class` round-trip correctly in `RepackagerTests`'s
other assertions) not surviving a write-then-read round trip through
CratonVM's jar/zip stack. Not confirmed whether the loss happens on write
(`JarOutputStream`/`Manifest.write()`) or read (`JarFile.getManifest()`/
`Manifest.read()`) — both are nominally real JDK bytecode with no obvious
CratonVM native override found in `native-builtins/src` for
`java.util.jar.Manifest` specifically, so if this is a CratonVM bug it's
most likely at the I/O layer underneath (`ZipFile`/`ZipEntry` byte
delivery) rather than in the Manifest parser itself — see Cluster 2, which
shows a separate, independent piece of evidence that raw zip byte-fidelity
is suspect in this same module's test run. Confirming would need a small
standalone repro: write a `Manifest` with `getEntries()` populated to a
`JarOutputStream`, read the raw bytes back, and diff against what
`java.util.jar.Manifest.write()` should produce on HotSpot.

Devtools' `ChangeableUrlsTests` (`spring-boot-devtools-residual-fails-cluster.md`,
Cluster 3) shows a related-looking `Manifest`/`JarFile.getManifest()`
resolution failure (there, the `Class-Path` **main** attribute) — flagged
there as a possible relative, not merged here since the specific manifest
data lost differs and no common code site is confirmed.

## Cluster 2 — nested-library CRC mismatch on repackage

| Class | Method(s) | Wall time |
|---|---|---:|
| `RepackagerTests` | 6 of the 9 failures in this run | 49.7s |

```
JUnit Jupiter:RepackagerTests:loaderIsWrittenFirstThenApplicationClassesThenLibraries()
  => java.util.zip.ZipException: Bad CRC checksum for entry BOOT-INF/lib/ba9ae46b-3c10-4667-993c-5045ee92ea6a.jar: a8ca9dc0 instead of e66fd2cd
       org.apache.commons.compress.archivers.zip.ZipArchiveOutputStream.handleSizesAndCrc(ZipArchiveOutputStream.java:1083)
       ...
       org.springframework.boot.loader.tools.AbstractJarWriter.writeNestedLibrary(AbstractJarWriter.java:160)
       org.springframework.boot.loader.tools.Packager$PackagedLibraries.write(Packager.java:569)
```

Same shape (different CRC values) for
`metaInfServicesFilesAreMovedBeneathBootInfClassesWhenRepackaged`,
`customLayoutFactoryWithoutLayout`, `layersIndex`,
`existingSourceEntriesTakePrecedenceOverStandardLibraries`, `classPathIndex`
— all writing a nested library jar with `STORED` (uncompressed,
pre-computed CRC) compression via `AbstractJarWriter.writeNestedLibrary`.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader-tools.org.springframework.boot.loader.tools.RepackagerTests.out.log`

**Root cause: not confirmed — hypothesis, grounded in the actual write
path.** `AbstractJarWriter.writeNestedLibrary` (`AbstractJarWriter.java:154-162`)
deliberately opens the same `Library`'s stream **twice**:

```java
public void writeNestedLibrary(String location, Library library) throws IOException {
	...
	new StoredEntryPreparator(library.openStream(), ...).prepareStoredEntry(entry);
	try (InputStream inputStream = library.openStream()) {
		writeEntry(entry, library, new InputStreamEntryWriter(inputStream));
	}
}
```

The first `openStream()` call is fully consumed by `StoredEntryPreparator`
(`AbstractJarWriter.java:315-349`) purely to compute the entry's real
`CRC32`/size ahead of time (`entry.setCrc(this.crc.getValue())`); the
second, independent `openStream()` call supplies the actual bytes written
to the `ZipArchiveOutputStream`. This two-pass design is correct and is
exactly what real Spring Boot does on HotSpot — it relies on
`library.openStream()` returning byte-for-byte identical content on both
calls. `ZipArchiveOutputStream.handleSizesAndCrc` (commons-compress, real
bytecode) then independently recomputes the CRC from whatever bytes it
actually received during the second pass and compares it against the
pre-computed value from the first pass — a mismatch here means the two
`openStream()` calls returned **different bytes** for the same
(temp-file-backed) `Library` within this one CratonVM process. Not
confirmed which of the two passes is wrong, nor why — would need to dump
an MD5/CRC of each independent read of the same library file to isolate
whether this is a caching/staleness issue in CratonVM's file I/O layer
(no such cache was found for raw file *content* — only for parsed
*manifest attributes*, `plain_manifest_cache`/`nested_manifest_cache` in
`native-builtins/src/lang_class.rs`, which is a different feature and
almost certainly not implicated here) or something else entirely.

## Cluster 3 — `ZipHeaderPeekInputStream` misreports EOF on a zero-length source

| Class | Method | Wall time |
|---|---|---:|
| `ZipHeaderPeekInputStreamTests` | `readMoreThanEntireStreamWhenStreamLengthIsZero()` | 0.19s (1/9 fail) |

```
JUnit Jupiter:ZipHeaderPeekInputStreamTests:readMoreThanEntireStreamWhenStreamLengthIsZero()
  => org.opentest4j.AssertionFailedError:
expected: -1
 but was: 8
       org.springframework.boot.loader.tools.ZipHeaderPeekInputStreamTests.readMoreThanEntireStreamWhenStreamLengthIsZero(ZipHeaderPeekInputStreamTests.java:121)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader-tools.org.springframework.boot.loader.tools.ZipHeaderPeekInputStreamTests.out.log`

The test wraps `new ByteArrayInputStream(new byte[0])` in
`ZipHeaderPeekInputStream` and calls `.read(new byte[8])`, expecting `-1`
(immediate EOF) — got `8`, exactly the size of the requested buffer, which
is the signature of some `read(byte[], int, int)` layer returning the
*requested length* instead of correctly detecting/propagating end-of-stream
on an empty source.

Tracing `ZipHeaderPeekInputStream`'s constructor
(`apps/spring-boot/loader/spring-boot-loader-tools/src/main/java/org/springframework/boot/loader/tools/ZipHeaderPeekInputStream.java:44-49`):
it does `this.headerLength = in.read(this.header)` against the empty
source (should be `-1` per the real `ByteArrayInputStream` contract), then
constructs `new ByteArrayInputStream(this.header, 0, this.headerLength)` —
i.e. `new ByteArrayInputStream(byte[4], 0, -1)`, a 3-arg constructor call
with a **negative length**. On real HotSpot, `ByteArrayInputStream`'s
constructor computes `count = min(offset+length, buf.length)`, which with
`length=-1` yields `count=-1`, and since `pos(0) >= count(-1)` is
immediately true, every subsequent read on that inner stream correctly
returns `-1` (EOF) without ever touching real data. The outer `read()`
method then falls through to `readRemainder`, which delegates to the real
(also-empty) underlying stream — also `-1`.

**Root cause: not confirmed.** No CratonVM native override for
`java.io.ByteArrayInputStream` was found in `native-builtins/src` (a
targeted grep for `ByteArrayInputStream` found only unrelated matches —
generic `InputStream`/native-IO registration tables, `TLS`, `keystore`,
etc. — nothing that looks like a `ByteArrayInputStream`-specific
fast-path), so this is most likely either (a) a subtly different
`ByteArrayInputStream(byte[], int, int)` construction result for a
negative `length` argument than the real JDK's documented arithmetic
above, or (b) a generic array-bulk-read fast path elsewhere in CratonVM's
real-IO stack that returns the caller's requested `len` instead of
correctly detecting `pos>=count`/true EOF for a zero-length backing array.
Neither has been confirmed against source this round — flagging as an
honest open question rather than guessing further.

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-loader-tools` | `org.springframework.boot.loader.tools.FileUtilsTests` |
| `loader/spring-boot-loader-tools` | `org.springframework.boot.loader.tools.RepackagerTests` |
| `loader/spring-boot-loader-tools` | `org.springframework.boot.loader.tools.ZipHeaderPeekInputStreamTests` |
