# ES HANG - current-dev --nojit ES812PostingsFormatTests

Status: FIXED (2026-07-13)

Class:
- `server org.elasticsearch.index.codec.postings.ES812PostingsFormatTests`

Original report:
- HotSpot: PASS, rc=0, 10.081s, 32 tests.
- CratonVM JIT: crashed, rc=139.
- CratonVM --nojit: exceeded the 600-second probe cutoff.

Root causes fixed:
- Unsafe's `ARRAY_*_BASE_OFFSET` statics were stored as `long` despite
  their `int` descriptor, corrupting JDK array helper stack typing.
- ByteBuffer limit/position and typed-view address handling did not match the
  real JDK layout, producing incorrect Lucene NIO seeks and reads.
- Synthetic gzip, inflater, deflater, and buffered-reader overrides conflicted
  with real JDK object layouts.
- `sun.nio.ch.Util`'s Java temporary direct-buffer cache can be observed
  concurrently by VM workers and returned null entries. It is now bypassed by
  a bounded native-thread-local pool of real `DirectByteBuffer` objects,
  rooted safely across moving GC.
- Aggressive JIT compilation of `Collections.indexedBinarySearch` could
  dispatch a lambda `apply` call through `java/lang/Object`; that narrow
  dispatcher is now interpreted pending a general invokeinterface PIC fix.

Verification on Azure host:
- Final --nojit run: `OK (32 tests)`, rc=0, 874.08s.
- Final JIT run: `OK (32 tests)`, rc=0, 855.11s.
- Concurrent temporary-buffer probe: buffer reuse verified; 8 workers x
  10,000 acquire/release operations completed without null buffers.

The no-JIT result is longer than the original 600-second cutoff because
`testDocIDRunEnd` performs 100 large write/merge iterations (millions of
interpreted operations); it completed CPU-active, with all 32 tests passing.
