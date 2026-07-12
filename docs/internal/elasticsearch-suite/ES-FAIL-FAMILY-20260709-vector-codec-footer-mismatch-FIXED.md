# ES failure family - vector codec footer/checksum corruption

Status: FIXED (2026-07-12) — root cause found and fixed; all 4 representative
classes pass matching HotSpot under both JIT-on and `--nojit`

## Fix summary (2026-07-12)

Root cause: the s2 synthetic `java/nio/ByteBuffer` native family
(`native-builtins/src/servlet.rs`) force-dispatches over real-JDK bytecode
for ~60 `ByteBuffer` methods. The bulk `get([BII)`/`get([B)`/`put([BII)`/
`put([B)` accessors located backing storage with
`s2_bb_arr(ctx, this).unwrap_or(src/dst)`. On a genuine real-JDK
`DirectByteBuffer` receiver (no heap array — `s2_bb_arr` returns `None`),
`unwrap_or` made the source/destination array copy into or out of
**itself**: a silent self-copy that dropped every write and left every read
returning whatever the caller's array already held (usually zero), while
`ByteBuffer.position()` still advanced and the call reported success. This
is the exact same shape as the already-fixed (2026-07-10)
`put(Ljava/nio/ByteBuffer;)` bug, just on the `byte[]`-bulk accessors
instead of the buffer-to-buffer one.

`IOUtil.read` routes every buffered `FileChannel` read through a temporary
DIRECT buffer and then bulk-`get`s it into a `byte[]`, so this exact path is
how Lucene's index footer/checksum bytes came back as zero: `CodecUtil`'s
footer magic/version/CRC32 fields are read via this bulk accessor on a
direct buffer, producing `actual footer=0` regardless of what was actually
on disk.

### What was NOT the mechanism (correcting the initial hypothesis)

The initial hypothesis (from static code inspection) was that
`s2_bb_get_byte`/`s2_bb_put_byte` — the single-byte primitives that
`getShort`/`putShort`/.../`getLong`/`putLong` all funnel through — lacked a
direct-buffer fallback entirely, silently breaking every scalar/typed
`ByteBuffer` accessor on a direct receiver. **Verified false** for a genuine
real-JDK `DirectByteBuffer`: `get(int)`, `put(int,byte)`, `getInt`/`putInt`,
`getLong`/`putLong`, etc. are all overridden by the *concrete*
`java.nio.DirectByteBuffer` class in real JDK 25, and CratonVM's
force-dispatch check (`force_native_over_real_jdk_bytecode`) only matches
`class_name == "java/nio/ByteBuffer"` — the *declaring* class resolved by
virtual dispatch for an overridden method is `java/nio/DirectByteBuffer`,
which the check does not match. Real bytecode runs instead, using the
buffer's real `address` field (confirmed non-zero and valid via reflection:
`address=0x2000ffb0c80`), and correctly round-trips values — proven with a
debug-instrumented build showing `s2_bb_get_byte`/`getInt(I)I`/`putInt(II)`'s
registered native closures are **never entered** for
`ByteBuffer.allocateDirect(...)` objects.

The `byte[]`-bulk methods (`get([BII)` etc.), by contrast, are declared and
implemented directly on the abstract `java/nio/ByteBuffer` class itself
(not overridden by `DirectByteBuffer`), so they DO match the force-dispatch
class-name check and DO run the broken s2 native — this is the real,
confirmed mechanism.

### The fix

Added the same direct-address fallback that fixed `put(ByteBuffer)`
(2026-07-10) to:

- `s2_bb_get_byte` / `s2_bb_put_byte` — the foundational single-byte
  primitives. These do not gate genuine `DirectByteBuffer` scalar accessors
  (real bytecode wins there, per above), but they ARE reached by
  slices/duplicates of a direct buffer and by typed buffer views
  (`asIntBuffer()` etc.) built over one, which stay storage-less without
  this fallback. `s2_bb_put_byte` (and the `write2`/`write4`/`write8`
  helpers built on it) were widened from `&dyn NativeContext` to
  `&mut dyn NativeContext` to reach `copy_to_native_memory`. On a failed
  native-memory access these low-level helpers stay panic-free and return
  the same benign default (0 / no-op) they already use for out-of-range
  indices — they have no `MethodCallResult` to propagate a Java exception
  through (called from ~15 call sites deep in the accessor chain).
- The 4 bulk methods `get([BII)`, `get([B)`, `put([BII)`, `put([B)` — these
  register top-level `MethodCallResult`-returning closures, so on a failed
  native-memory access they throw `IllegalStateException`, matching the
  existing `put(ByteBuffer)` pattern exactly.

`native-builtins/src/tests_extracted.rs` has a sibling copy of this s2
family, but the entire file is wrapped in `#[cfg(test)] mod tests { ... }`
— it only compiles under `cargo test` and is never reachable via real
dispatch (the runtime registry only wires up `servlet.rs`'s
registrations). Left unfixed; noted as a separate, non-blocking
observation, not a required fix.

### Verification

Representative classes (seed `B17AC9D3E1F2A0C4`), full class runs, CratonVM
`--java-home` real-JDK mode:

| Class | Before | After (JIT-on) | After (`--nojit`) | HotSpot |
|---|---|---|---|---|
| `ES813FlatVectorFormatTests` | FAIL, `codec footer mismatch: actual footer=0` | `OK (53 tests)` | `OK (53 tests)` | `OK (53 tests)` |
| `es93.ES93FlatVectorFormatTests` | FAIL (same signature) | `OK (106 tests)` | `OK (106 tests)` | matches |
| `es93.ES93FlatBFloat16VectorFormatTests` | FAIL (same signature) | `OK (53 tests)` | `OK (53 tests)` | matches |
| `diskbbq.ESNextOversamplingMetaTests` | FAIL (same signature) | `OK (53 tests)` | `OK (53 tests)` | matches |

A minimal direct `ByteBuffer` probe (`ByteBuffer.allocateDirect(16)`,
`order(LITTLE_ENDIAN)`, `put(byte[],0,8)`/`get(byte[],0,8)`) reproduced the
exact bug pre-fix (bulk round-trip returned all zeros while
`getInt`/`putInt`/`getLong`/`putLong` on the same object round-tripped
correctly) and passes post-fix, matching HotSpot, under both JIT-on and
`--nojit`.

Regression check: `native-builtins`'s full unit-test suite (2985 tests) —
pre-fix and post-fix failure sets compared directly; the 3 tests that
differed between runs (`bootstrap_property_fallback_tests::*`,
`lang_class::tests::jspecify_type_use_field_annotations_reach_annotated_type`)
all pass individually and single-threaded, confirming pre-existing
parallel-test-harness flakiness (env-var races), not a regression from this
fix. `vm` crate's `interpreter_tests.rs` ByteBuffer/CharBuffer/IntBuffer/
LongBuffer suite (`test_s48_bb_*`/`test_s48_cb_*`/`test_s48_ib_*`/
`test_s48_lb_*`/`test_s48_e2e_*`, 26 tests) — all pass. Spot-checked 5 other
ES vector-codec classes not in this family
(`ES813Int8FlatVectorFormatTests`, `ES815HnswBitVectorsFormatTests`,
`OptimizedScalarQuantizerTests`, `BQVectorUtilsTests`, `BFloat16Tests`) — 3
pass cleanly, 2 fail on an unrelated, pre-existing native-access
bootstrap issue confirmed identical on the unpatched baseline (not a
regression).

**Does not** fix item 4 of
`docs/known-issues/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps.md`
(`ChecksumIndexInput.getChecksum()` divergence from HotSpot on identical
bytes) — reproduced with a fresh probe (`ProbeNIOFS2.java`: write via
`NIOFSDirectory`, read back through `openChecksumInput`) showing the exact
same wrong checksum value both before and after this fix
(`170114997` vs HotSpot's `2329538857`), while the read-back bytes
themselves match. Confirmed separate root cause, remains open — see that
doc.

## Reopened on current dev (2026-07-11, superseded by the fix above)

The original float-array `Unsafe.copyMemory` fix remains present in
`native-builtins/src/lib.rs`, but the same observable vector-file corruption
still occurs on current `dev`. This note is therefore open again; the current
evidence does not prove that the original float-array fix is wrong, only that
it did not cover every path that can produce a zero or truncated footer.

Current-dev focused probe:

- CratonVM source and binary: `d274d898c43a4ca07ac877ba85543d153d2ea83c`
  (`cratonvm-es-focused-currentdev-20260711-172542`).
- Class: `org.elasticsearch.index.codec.vectors.ES813FlatVectorFormatTests`
  (`others` index 1339 in the compiled fixture).
- HotSpot: PASS, 53 tests, 0 failures.
- CratonVM JIT on: FAIL, 14 tests, 3 failures, including
  `CorruptIndexException: codec footer mismatch (file truncated?): actual footer=0`.
- CratonVM JIT off: the same FAIL with the same footer mismatch.

The failed test is `testMultiClose`. Both CratonVM modes also report the
randomized-testing suite deadline as exceeded despite the runner recording a
short class duration. This is an additional symptom, not evidence that the
footer mismatch is merely a timeout artifact.

Next step: use a minimal `FloatBuffer.put(float[])` plus Lucene
`Directory` write/read probe to compare the on-disk bytes, the bytes read
through CratonVM NIO, and the footer presented to Lucene. Keep this separate
from the broader vector performance timeout family.

Fix summary:
- Root cause was CratonVM's raw `Unsafe.copyMemory` primitive-array byte reader treating `float[]` elements as `Value::Int`; real Java float arrays store `Value::Float`, so `FloatBuffer.put(float[])` copied all-zero bytes into byte-backed vector files.
- `native-builtins/src/lib.rs` now encodes `Value::Float` with `f32::to_bits()` in `unsafe_array_read_bytes` and reconstructs `Value::Float` in `unsafe_array_write_bytes`.
- This fixes JDK `FloatBuffer.putArray` / `ScopedMemoryAccess.copyMemory` for byte-buffer float views, the path Lucene vector writers use.

Validation:
- `/tmp/bytebuffer-floatview-r2-1783666407`: `ByteBuffer.allocate(...).order(...).asFloatBuffer().put(float[])` now writes HotSpot-matching BE/LE bytes and reads back `0.074157976`, `-1.0`.
- `/tmp/cratonvm-es93-flatvector-full-floatview-r2-1783666566`: `ES93FlatVectorFormatTests` passed, `OK (106 tests)`.
- `/tmp/cratonvm-ES93FlatBFloat16VectorFormatTests-floatview-r2-1783666687`: `OK (53 tests)`.
- `/tmp/cratonvm-ESNextOversamplingMetaTests-floatview-r2-1783666687`: `OK (53 tests)`.

Signals:
- `org.apache.lucene.index.CorruptIndexException: checksum status indeterminate`
- `org.apache.lucene.index.CorruptIndexException: codec footer mismatch (file truncated?): actual footer=0 vs expected footer=-1071082520`

Full rerun count:
- Run: `es-nonpassed-rerun-20260708-191002`
- Direct checksum/footer FAIL rows: 3 of 1064 total FAIL rows.
- Classes: `ESNextOversamplingMetaTests`, `ES93FlatBFloat16VectorFormatTests`, `ES93FlatVectorFormatTests`.

Representative class:
- `server org.elasticsearch.index.codec.vectors.es93.ES93FlatVectorFormatTests`

Current-dev proof:
- Probe run: `es-faildocs-probe-20260709-073704`
- HotSpot: PASS, rc=0, 5.287s.
- CratonVM JIT: FAIL, rc=1, 5.022s, `codec footer mismatch`.
- CratonVM --nojit: FAIL, rc=1, 67.567s, 18 JUnit failures including `codec footer mismatch`, zero-score assertions, and array value mismatches.
- Hang timeout for the probe run was 600s; this class did not hang.

Representative --nojit failure details:
- `CorruptIndexException: codec footer mismatch (file truncated?): actual footer=0 vs expected footer=-1071082520`
- `AssertionError: expected:<1.0> but was:<0.0>`
- `ArrayComparisonFailure: values differ ... expected:<0.012180211> but was:<0.0>`
- `AssertionError: encoding=FLOAT32 expected:<74.63681667204946> but was:<0.0>`

Interpretation:
- HotSpot reads back valid vector codec data from the same fixture, while CratonVM returns zeroed or truncated footer/vector data.
- Reproduction under `--nojit` points to runtime IO, byte-buffer, memory, or vector-codec helper behavior rather than a JIT-only issue.
- This overlaps symptomatically with the vector score-zero family, but the Lucene footer/checksum failure is a distinct persistence/readback integrity signal and should stay tracked separately.

Next investigation:
- Start from a minimal Lucene `Directory` write/read probe that writes footer-bearing vector codec files and compares raw bytes before Lucene validation.
- Capture whether the zero footer is already present on disk, appears through CratonVM's NIO read path, or is introduced by a buffer/view conversion.

## Current-dev 120s full non-passed rerun

- Run: `es-nonpassed-currentdev-20260709-082115`
- `ES93FlatVectorFormatTests` remains one of only two Java-level FAIL rows after the current-dev rerun.
- Result: FAIL, rc=1, 4.807s.
- Note: `CorruptIndexException: codec footer mismatch (file truncated?): actual footer=0 vs expected footer=-1071082520`.
- Most neighboring vector classes now crash rc=139 before reaching this Java-level assertion, so this row is still the clearest current proof for the footer/checksum family.
