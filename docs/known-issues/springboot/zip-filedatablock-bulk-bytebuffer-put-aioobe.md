# `FileDataBlock$FileAccess.read`: bulk `ByteBuffer.put(ByteBuffer)` throws `ArrayIndexOutOfBoundsException` — breaks the entire `spring-boot-loader` module (9/9 classes) and any Zip64 handling elsewhere

**Status: OPEN, characterized. Severity: HIGH (universal within the affected
module — not an edge case).**

Found while triaging `FAIL`s from the first full Spring Boot suite run (see
[[project_spring_boot_suite_runner_20260711]]). Every class in
`loader/spring-boot-loader` fails (`ZipContentTests`,
`NestedJarFileTests`, `SecurityInfoTests`, `UrlJarFileFactoryTests`,
`UrlNestedJarFileTests`, `NestedUrlConnectionTests`, `NestedPathTests`,
`NestedFileSystemProviderTests`, `FileDataBlockTests`,
`VirtualZipDataBlockTests`) — this is Spring Boot's own executable-jar
(nested-jar) loader, exercised by essentially every `java -jar
spring-boot-app.jar` launch. **28/29 methods in `ZipContentTests` fail**,
including trivial ones (`sizeReturnsNumberOfEntries`,
`getEntryWhenPresentReturnsEntry`, `getCommentReturnsComment`) — this is not
limited to the Zip64-edge-case tests (`openWhenZip64ThatExceedsZipSizeLimitOpensZip`
etc.), it's any zip/jar read through this code path:

```
java.lang.ArrayIndexOutOfBoundsException
   jdk.internal.misc.ScopedMemoryAccess.copyMemoryInternal(ScopedMemoryAccess.java:148)
   jdk.internal.misc.ScopedMemoryAccess.copyMemory(ScopedMemoryAccess.java:130)
   java.nio.ByteBuffer.putBuffer(ByteBuffer.java:1143)
   java.nio.ByteBuffer.put(ByteBuffer.java:1114)
   org.springframework.boot.loader.zip.FileDataBlock$FileAccess.read(FileDataBlock.java:196)
   org.springframework.boot.loader.zip.FileDataBlock.read(FileDataBlock.java:84)
   org.springframework.boot.loader.zip.DataBlock.readFully(DataBlock.java:64)
   org.springframework.boot.loader.zip.ZipEndOfCentralDirectoryRecord.locate(...)
```

`FileDataBlock$FileAccess.read` reads a file segment via `FileChannel.read`
into a temporary direct buffer, then bulk-copies it into the caller's heap
`ByteBuffer` via `dst.put(tempDirectBuffer)` — a `ScopedMemoryAccess.copyMemory`
call. That copy throws `ArrayIndexOutOfBoundsException` instead of
completing.

## Likely connection to prior work

This is the same call shape (`ScopedMemoryAccess.copyMemory` between a
direct/arena buffer and a heap buffer) as the already-fixed "mixed
heap↔off-heap `Unsafe.copyMemory` silently dropped" bug documented in memory
under `reference_server_socket_gap` (UPDATE 4, `native_unsafe_copy_memory_consolidated`
in `native-builtins/src/unsafe_natives.rs`) — that fix made the mixed-direction
copy actually move bytes instead of silently no-op'ing, but did not
necessarily add correct bounds/length validation for every call shape. This
looks like a regression or an unaddressed edge in the *same* function family
rather than a fresh, unrelated bug — worth checking there first
(`native_unsafe_copy_memory_consolidated`, and whatever backs
`jdk/internal/misc/ScopedMemoryAccess.copyMemory0` specifically, which may be
a different native than plain `Unsafe.copyMemory`).

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for loader/spring-boot-loader's org.springframework.boot.loader.zip.ZipContentTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```
Any test in that class reproduces it (not just the Zip64-specific ones), so
`getCommentReturnsComment` or `sizeReturnsNumberOfEntries` are the smallest/fastest repros in the cluster.
