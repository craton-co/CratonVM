# ES FAIL family - FloatBuffer abstract receiver no-Code in vector codecs

Status: FIXED (branch `fix/es-floatbuffer-abstract-receiver-nocode-20260710`)

Date observed: 2026-07-10 (run `esfull-20260710-083851`)
Date fixed: 2026-07-10/11

## Summary

Several vector-codec test classes failed under CratonVM because typed NIO
buffer methods that are `public abstract` in real JDK 25 (unlike
`ByteBuffer`, where they are concrete bytecode) had no override anywhere on
the receiver's class hierarchy:

```text
java.lang.AbstractMethodError: method java/nio/FloatBuffer.order()Ljava/nio/ByteOrder; has no Code attribute
```

and, once `order()` was partially fixed (see below), the same shape of bug
resurfaced for other abstract methods/types:

```text
java.lang.AbstractMethodError: method java/nio/FloatBuffer.put(IF)Ljava/nio/FloatBuffer; has no Code attribute
java.lang.AbstractMethodError: method java/nio/IntBuffer.order()Ljava/nio/ByteOrder; has no Code attribute
```

## Root cause

`FloatBuffer`/`IntBuffer`/`LongBuffer`/`DoubleBuffer`/`ShortBuffer` each
declare `order()`, `slice()`, `slice(int,int)`, `duplicate()`,
`asReadOnlyBuffer()`, `get()`/`get(int)`, `put(x)`/`put(int,x)`, and
`compact()` as `public abstract` — the concrete implementation lives on
subclasses like `HeapFloatBuffer` or the `ByteBufferAsFloatBufferB/L` view
classes. `ByteBuffer.asFloatBuffer()` (and the sibling `asXxxBuffer()`
methods, `native-builtins/src/servlet.rs`'s `s2_view_buf_fn!` macro) and the
typed-buffer `allocate()`/`wrap()` factories both allocate their returned
object via `alloc_concurrent_synthetic`/`alloc_typed_buffer` against the
LITERAL abstract class name (e.g. `java/nio/FloatBuffer`), not a concrete
subclass. An `invokevirtual` for any of the abstract methods above against
that receiver therefore resolved to the Code-less abstract declaration and
threw `AbstractMethodError`.

Confirmed via `CRATONVM_DBG_NOCODE=1`: `recv_cid=2478 recv_class=java/nio/FloatBuffer`.

A same-day commit (`1c60eb0ffa`, "Fix Elasticsearch sliced IVF no-JIT hang")
had already registered `FloatBuffer.order()` directly on the abstract class
in `native-builtins/src/lib.rs` (`register_essential_natives`), which
resolved the majority of the family's `order()` signal — but two residuals
remained: `FloatBuffer.put(int,float)` (hit by
`ES814HnswScalarQuantizedVectorsFormatTests.testRescoreUsesRawVectorSlice`,
which reads a raw vector slice via `ByteBuffer.asFloatBuffer()`) and
`IntBuffer.order()` (hit by `PreconditionerTests`).

**Two dead-code traps found while fixing this:**

1. `native-io/src/lib.rs`'s `register_nio_natives` — which already had
   `get`/`put`/`compact` registered for every typed buffer, and looked like
   the natural place to add the missing methods — is only called under
   `#[cfg(feature = "synthetic-jdk")]`, which is **off by default**. Any fix
   added there is dead code in the real-JDK build actually under test.
2. The methods that ARE active in real-JDK mode for these receivers live in
   a second, independent implementation:
   `native-builtins/src/servlet.rs::register_s2_bytebuffer` (called from the
   unconditional `register_essential_natives`), which only had `IntBuffer`
   `get`/`put` implemented and nothing else (no `order`, `slice`,
   `duplicate`, `asReadOnlyBuffer`, or `get`/`put`/`compact` for
   `Long`/`Short`/`Float`/`Double`) — so most of these methods were never
   reachable at all, not merely mis-registered.

## Fix

`native-builtins/src/servlet.rs::register_s2_bytebuffer` (the active
real-JDK path): added a `s2_typed_buffer_view_fns!` macro generating
`get`/`get(int)`/`put`/`put(int,x)`/`order`/`slice`/`slice(int,int)`/
`duplicate`/`asReadOnlyBuffer`/`compact` for a given class/element-width,
using the existing `s2_bb_read{2,4,8}`/`write{2,4,8}` byte-level accessors
and the `BB_MARK`-encoded byte-start offset convention `s2_view_buf_fn!`
already uses for view buffers. Registered for
`Long`/`Short`/`Float`/`Double`Buffer in full; `IntBuffer` keeps its
existing `get`/`put` and only gained the previously-missing
`order`/`slice`/`slice(II)`/`duplicate`/`asReadOnlyBuffer`/`compact`.

`native-io/src/lib.rs::register_nio_natives` (synthetic-jdk feature): same
methods added via an analogous `tb_abstract_view_fns!` macro, for parity
between real-JDK and synthetic-jdk modes (per project convention — both
modes must keep working).

## Verification

Built `cratonvm-es-floatbuffer-fix2` (Linux/Azure host,
`/data/data/wt-es-floatbuffer-nocode-20260710`) and re-ran all 22 classes
from the original family list (20 FAIL + 2 HANG) with
`CRATONVM_DBG_NOCODE=1`, `--nojit`:

- **All 22 classes: zero buffer no-Code hits** (`grep -c DBG_NOCODE` == 0
  for every class, both the `recv_class=java/nio/*Buffer` filter and the
  unfiltered count).
- `PreconditionerTests`: was 1 failure (the `IntBuffer.order()`
  AbstractMethodError) → now `OK (1 test)`.
- `ES93HnswScalarQuantizedBFloat16VectorsFormatTests` (formerly HANG): now
  `OK (57 tests)`.
- `ES93ScalarQuantizedBFloat16VectorFormatTests` (formerly HANG): now
  `OK (54 tests)`.
- The other 19 classes run to completion with no AbstractMethodError; each
  has some remaining test failures, but all are a distinct, pre-existing
  "expected `<X>` but was `<0.0>`" vector-similarity/dot-product bug and (in
  JIT mode) a JIT-only SIGSEGV — see
  [`ES-FAIL-FAMILY-20260710-vector-scoring-zero-and-jit-segfault.md`](../../known-issues/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-scoring-zero-and-jit-segfault.md),
  confirmed present on the UNMODIFIED baseline binary (before this fix) and
  unaffected by it.

Re-ran three classes that hit the harness's 90s per-class timeout under host
load (`ESNextOversamplingMetaTests`, `ESNextDiskBBQVectorsFormatTests`,
`ES93ScalarQuantizedVectorsFormatTests`) individually with a 240s timeout:
all complete, zero no-Code hits — the timeouts were host contention (load
average ~9-10 on the shared Azure build host), not hangs.

## Original repro (superseded)

- Run: `esfull-20260710-083851`, `esprobe-nocode-floatbuffer-20260710`,
  `esprobe-hotspot-floatbuffer-20260710` (Windows box; evidence paths no
  longer exist).
