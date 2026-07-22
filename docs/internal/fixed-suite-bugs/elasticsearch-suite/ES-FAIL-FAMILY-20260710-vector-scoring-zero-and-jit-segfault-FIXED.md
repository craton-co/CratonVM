# ES vector-codec family: dot-product scores read back as 0.0, and a JIT-only SIGSEGV

Status: FIXED (2026-07-11)

Observed: 2026-07-11, while verifying the fix for
[`ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md`](ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md).

Not a duplicate of that doc: these two symptoms were unmasked by, but
independent of, the abstract-receiver no-Code bug — confirmed present on the
**unmodified pre-FloatBuffer-fix baseline binary** too.

## Root cause

`alloc_concurrent_synthetic()` resolves the real (abstract)
`java.nio.{Int,Long,Short,Float,Double}Buffer` class in real-JDK mode, which
declares no fields of its own beyond `Buffer`'s own six
(`mark,position,limit,capacity,address,segment`) — the SAME slot count the
`native-builtins/src/servlet.rs` `s2_*` indexed-slot convention uses for a
pure-synthetic `ByteBuffer` layout (`array,pos,limit,cap,mark,order`).
`ctx.set_field` performs descriptor-aware coercion: writing an `Object` (the
backing `byte[]`) into slot 0 — really `Buffer.mark`, an `int` — silently
truncated it to garbage. `s2_bb_synthetic_layout()`, which exists to
distinguish the two 6-slot shapes, only checked field *count*, so it
misidentified every typed-buffer view as pure-synthetic too, and
`s2_bb_set_order()` then clobbered slot 5 (really `Buffer.segment`, the one
Object-typed field — and where the fix stashes the array) with an int order
flag on every `ByteBuffer.asFloatBuffer()`-style view.

Net effect: every value read through a typed-buffer view — the standard way
ES/Lucene vector codecs read stored float vectors — came back as exactly
`0.0`, and JIT-compiled code hitting the same corrupted state crashed
outright (SIGSEGV) instead of just reading zero.

A second, independent gap compounded this: `get(T[],int,int)`/
`put(T[],int,int)` are *concrete* (not abstract) real JDK 25 bytecode on
every typed buffer — `FloatBuffer.getArray`/`putArray` read/write via
`this.address` + `ScopedMemoryAccess` directly for any length beyond a
trivial few elements, completely bypassing virtual dispatch to the
single-element accessors. Fixing the array-storage bug alone was not
sufficient for the dominant `buffer.get(vec, 0, dims)` bulk-read access
pattern; the bulk methods needed their own natives, force-listed in
`vm/src/runtime/interpreter.rs::force_native_over_real_jdk_bytecode` since
real bytecode already exists for those signatures.

## Fix

`native-builtins/src/servlet.rs`:
- `s2_bb_synthetic_layout()`: also checks the class name — a real-JDK-loaded
  `Int/Long/Short/Float/DoubleBuffer` is never pure-synthetic regardless of
  field count.
- `s2_bb_arr()`: added a `BB_SEGMENT_SLOT` (index 5) fallback for the array
  reference on these views.
- `s2_view_buf_fn!` (`ByteBuffer.asXxxBuffer()`) and
  `s2_typed_buffer_view_fns!`'s `slice`/`slice(II)`/`duplicate`: write/
  propagate the array via `BB_SEGMENT_SLOT` instead of the coercion-broken
  slot 0; `array()` reads it via `s2_bb_arr`.
- `s2_bb_order()`/`s2_bb_set_order()`: typed-buffer views without a
  `bigEndian` field now track their order flag in the real `mark` slot
  (index 0 — genuinely free once the array moved to slot 5, and int-typed so
  no coercion trap) instead of colliding with the array in slot 5.
- Added bulk `get(T[],int,int)`/`put(T[],int,int)` natives for
  Int/Long/Short/Float/DoubleBuffer, force-listed in
  `vm/src/runtime/interpreter.rs`.

## Verification

An isolated `ByteBuffer`/`asFloatBuffer()` probe (heap, `wrap(byte[])`, and
explicit `LITTLE_ENDIAN` cases) now matches real JDK 25 exactly.

Across the 22 classes shared with the FloatBuffer-family doc:

- **14/22 classes now fully pass** under `--nojit` (`ES813Flat*`,
  `ESNextOversamplingMetaTests`, `PreconditionerTests`, `ES940v1/v2`,
  `ES816Binary*`, `ES93BinaryQuantized`, `ES93HnswScalarQuantized`,
  `ES93HnswVectors`, `ES93ScalarQuantized*`) — every one of these previously
  had 9–16 zero-score assertion failures.
- Remaining per-class failures dropped to 1–2 each, and every one checked is
  a **confirmed distinct, unrelated, pre-existing bug**: a Panama downcall
  arity mismatch (`ES818Binary*`, `ES93*BFloat16*`,
  `ES93HnswBinaryQuantized*`), an unrelated `ClassCastException:
  ArrayList cannot be cast to String` (`ES93FlatVectorFormatTests`), and an
  fd-exhaustion cascade (`ES814HnswScalarQuantizedVectorsFormatTests`) — none
  touch buffer/vector-value reading.
- **The JIT-on SIGSEGV, which reproduced on ~20/22 classes both before this
  fix and on the unmodified baseline, no longer reproduces on any of them**
  (confirmed with a clean, non-contended sweep — zero crashes across all 22
  classes under JIT).

## Evidence

Azure host `/data/data/wt-es-vectorscore-jitsegv-20260711`, branch
`fix/es-vectorscore-jitsegv-20260711`, binary
`target/release/cratonvm-vectorscore-final`. Sweep outputs:
`/data/data/es-nocode-verify-1877323/` (JIT), earlier `--nojit` sweep at
`/data/data/es-nocode-verify-1760429/`.
