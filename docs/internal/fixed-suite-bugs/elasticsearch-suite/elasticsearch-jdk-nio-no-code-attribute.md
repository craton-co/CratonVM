# Elasticsearch JDK NIO abstract methods report no Code attribute

Status: fixed (branch `fix/es-jdk-nio-no-code-attribute`)

Date observed: 2026-07-02
Date fixed: 2026-07-02

## Summary

Several engine and query tests failed under CratonVM because two real-JDK
abstract methods were reached with no native override registered anywhere on
the receiver's class hierarchy:

```text
java.lang.AbstractMethodError:
method java/nio/file/spi/FileSystemProvider.getFileStore(Ljava/nio/file/Path;)Ljava/nio/file/FileStore; has no Code attribute
```

```text
java.lang.AbstractMethodError:
method java/nio/Buffer.isReadOnly()Z has no Code attribute
```

## Root causes

**`getFileStore`**: CratonVM's `FileSystem.provider()` returns a synthetic
object stamped with the literal abstract class
`java/nio/file/spi/FileSystemProvider` (not a concrete `sun.nio.fs.*`
subclass). `Files.getFileStore(path)`'s real bytecode is
`return provider(path).getFileStore(path);`, so the `invokevirtual` landed on
that synthetic instance. Every other abstract `FileSystemProvider` method
reachable from test code (`isSameFile`, `newFileSystem` x2,
`getFileAttributeView`, `newByteChannel`, etc.) already had a native
registered directly on the class — `getFileStore` was simply missing from
that set, and the natives registered on `java/nio/file/Files` in
`register_p71_files_bridge` never fire in real-JDK mode (`Files.getFileStore`
has real Code, so bytecode runs instead of the shadow).

Confirmed via `CRATONVM_DBG_NOCODE=1`:
`recv_cid=1676 recv_class=java/nio/file/spi/FileSystemProvider`.

**`isReadOnly`**: `CharBuffer.isReadOnly()`/`isDirect()` were never
registered anywhere (every sibling typed-buffer class — Int/Long/Short/Float/
DoubleBuffer — has them registered in `register_s2_bytebuffer`, but
`CharBuffer` was left out of that loop). Several native call sites allocate
`CharBuffer` objects stamped as the literal abstract `java/nio/CharBuffer`
class (`p62_alloc_char_buffer` correctly uses the concrete
`HeapCharBuffer`, but other sites — `CharBuffer.wrap([C])`,
`CharBuffer.wrap(CharSequence)`, `ByteBuffer.asCharBuffer()` — allocate the
abstract class directly), so `isReadOnly()`/`isDirect()` had no override to
resolve to anywhere up the chain.

Confirmed via `CRATONVM_DBG_NOCODE=1`:
`recv_cid=313 recv_class=java/nio/CharBuffer`.

## Fix

- `../../../../native-builtins/src/phases_late.rs`: register `getFileStore` directly on
  `java/nio/file/spi/FileSystemProvider`, returning a new synthetic
  `java/nio/file/FileStore` (helper `p57_alloc_file_store`) instead of null —
  every `FileStore` abstract accessor (`name`, `type`, `isReadOnly`,
  `getTotalSpace`/`getUsableSpace`/`getUnallocatedSpace`,
  `getBlockSize` (concrete default in real JDK but throws
  `UnsupportedOperationException` unconditionally — ES's
  `FsDirectoryFactory.blockSize` calls it directly), `supportsFileAttributeView`
  x2, `getFileStoreAttributeView`, `getAttribute`, `toString`) is registered
  so no follow-on call into the returned object can hit the same class of bug.
- `../../../../native-builtins/src/phases_late.rs`: register `isReadOnly`/`isDirect` on
  `java/nio/CharBuffer` directly (reads the real-JDK `isReadOnly` field when
  present, mirroring the existing `hasArray` pattern).
- `../../../../native-io/src/lib.rs`: register `isReadOnly`/`isDirect` as a catch-all
  fallback on the abstract `java/nio/Buffer` class itself, alongside the
  other 8 accessors (`position`/`limit`/`capacity`/`remaining`/
  `hasRemaining`/`clear`/`flip`/`rewind`) already registered there — closes
  the same gap for any other buffer type allocated directly against an
  abstract class name in the future.

## Verification

Re-ran the repros below against a release build with the fix
(`C:\craton\CratonVM-esnio-fix`, branch `fix/es-jdk-nio-no-code-attribute`):

- `InternalEngineFieldInfoCachingTests` (getFileStore, JIT on): the
  `AbstractMethodError` is gone; the test now proceeds into engine/node setup.
- `NoOpEngineTests` (getFileStore, JIT on): same — no `AbstractMethodError`
  in any of its 5 methods.
- `FunctionScoreEquivalenceTests` (isReadOnly, JIT on): went from
  `Tests run: 0, Failures: 2` (crashed in static init) to
  `Tests run: 3, Failures: 1` — no `AbstractMethodError`; the one remaining
  failure is the already-tracked
  [`elasticsearch-randomizedcontext-per-thread-null.md`](../known-issues/elasticsearch-randomizedcontext-per-thread-null.md)
  issue (`RandomizedContext.getPerThread()` returns null).
- `ES815BitFlatVectorFormatTests` (isReadOnly, no-JIT): no
  `AbstractMethodError`; remaining failure is an unrelated
  `NoSuchMethodError: java/lang/invoke/SegmentVarHandle.get(...)` gap (Panama
  MemorySegment/VarHandle support), and this class already fails on HotSpot
  too per the original no-JIT evidence.

## Residual: node-lock cascade is a symptom of the RandomizedContext bug, not this one

With both `AbstractMethodError`s fixed, `InternalEngineFieldInfoCachingTests`
and `NoOpEngineTests` (2 of the 5 tests the original doc said "cascade into
node-lock failures") now fail deterministically — even on a fresh run with no
stale lock files on disk — with:

```text
java.lang.IllegalStateException: failed to obtain node locks, tried [X, X]
Caused by: org.apache.lucene.store.LockObtainFailedException: Lock held by this virtual machine
```

`NodeEnvironment`'s error message is `Arrays.toString(environment.dataDirs())`
— the SAME path appears twice because `ESTestCase.tmpPaths()` calls
`createTempDir()` 1-3 times (`TestUtil.nextInt(random(), 1, 3)`) and, in this
run, returned the identical path twice instead of two distinct temp
directories. `createTempDir()`'s naming is per-thread/RandomizedContext-scoped
state — the same subsystem already tracked as broken in
[`elasticsearch-randomizedcontext-per-thread-null.md`](../known-issues/elasticsearch-randomizedcontext-per-thread-null.md).
This is a distinct, pre-existing bug this fix unmasked rather than caused;
left open there rather than duplicated here.

## Original repro commands (evidence paths no longer exist; superseded)

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1398 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-jdk-nio-no-code-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```
