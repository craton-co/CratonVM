# ES FAIL family - vector codec exceptions with corrupted Throwable cause output

Status: FIXED (2026-07-10) — all 3 rows PASS; both root causes found and fixed

## Fix summary (2026-07-10, second follow-up session)

All three family rows now pass with the family seed (`B17AC9D3E1F2A0C4`,
`--nojit` and JIT-on, Linux build host, ES server-module harness at
`/data/data/es-jit-deopt-gc-bundle-20260708-214648/elasticsearch`):

- `ES815BitFlatVectorFormatTests` — PASS, `OK (6 tests)` (was AIOOBE).
- `ES93HnswBFloat16VectorsFormatTests` — PASS, `OK (17 tests)` (was AIOOBE).
- `ES93FlatBFloat16VectorFormatTests` — PASS, `OK (53 tests)` (was
  `BufferUnderflowException` + `Caused by: java.lang.Object`).

Three distinct bugs stacked under this doc's face (on top of the
`RandomizedContext.current()` regression fixed by the first follow-up
session, see
`docs/internal/fixed-suite-bugs/randomizedcontext-current-null-instead-of-throw-FIXED.md`):

### 1. `ByteBuffer.put(ByteBuffer)` silently no-oped when either side is a direct buffer

`native-builtins/src/servlet.rs`'s live s2 `put(Ljava/nio/ByteBuffer;)`
native returned early — copying nothing and advancing neither position —
whenever a side had no heap array (`s2_bb_arr == None`), which is exactly
what a real-JDK `DirectByteBuffer` looks like (`hb == null`, storage at
`address`). `IOUtil.read` routes **every buffered `FileChannel` read**
through a temporary direct buffer and then `dst.put(directSrc)`, so file
reads "succeeded" (correct byte count returned by `pread`) while delivering
ZERO bytes into an unmoved heap buffer. Lucene's
`BufferedIndexInput.refill()` then `flip()`ed an empty buffer and the first
`readByte()` threw `BufferUnderflowException` — the doc's testMultiClose
face (testMultiClose is the one test whose seed-chosen directory is
FS-backed; forcing `-Dtests.directory=NIOFSDirectory` produced 35 identical
underflows pre-fix, and a 40-line probe — `NIOFSDirectory` write 5000 bytes,
`openInput().readByte()` — reproduced it standalone).

Fixed by teaching the copy loop to read/write direct buffers through
`address` + `copy_from/to_native_memory` (same resolution as native-io's
`directbuffer_address`), in both the live `servlet.rs` registration and its
`tests_extracted.rs` sibling.

### 2. `Throwable.addSuppressed` wrote the suppressed list into the real-JDK `cause` slot — the `Caused by: java.lang.Object` corruption

`native_throwable_add_suppressed` (`native-builtins/src/lang_misc.rs`)
stored its suppressed ref-array in POSITIONAL field 2. That matches only an
old 3-field synthetic throwable layout; on the real-JDK `Throwable` layout
(`backtrace`(0), `detailMessage`(1), `cause`(2), ...) field 2 is **`cause`**.
`CodecUtil.checkFooter(in, priorException)` calls
`priorException.addSuppressed(t)` before `IOUtils.rethrowAlways`, so the
BufferUnderflowException's self-sentinel cause was overwritten with an
`Object[]` (element class id 0 = `java/lang/Object` → printed as
`Caused by: java.lang.Object`; the trace printer then walked the array as a
Throwable, which is what tripped the `gen_heap::read_slot` corrupt-Value-cell
guard seen in the same runs).

This fully explains the previous session's `CRATONVM_DBG_CAUSE` evidence and
**refutes the GC self-forward relocation theory** in the update below: the
mystery cause value (hash `493742`, never seen on any WRITE line) was the
suppressed-array write bypassing `write_throwable_cause` entirely — plain
deterministic slot aliasing, no GC involvement (consistent with the failure
being deterministic in 3s runs).

Fixed on dev by the concurrent session's `8a4064f8` ("fix:
Throwable.addSuppressed() clobbered cause via wrong field index" — resolves
`suppressedExceptions` by NAME, keeps the ref-array representation; found
independently via a hardware data-write breakpoint on the victim's cause
slot, see
`docs/internal/fixed-suite-bugs/throwable-addsuppressed-clobbers-cause-FIXED.md`).
This session root-caused the same bug from the slot-aliasing side and
carried an equivalent rewrite (real-JDK `List` semantics on the named
field); the landed name-based fix was kept in the merge, the duplicate
rewrite dropped.

### 3. (unmasked by #1) `order(LITTLE_ENDIAN)` ignored on real-JDK buffers — byteswapped `getShort`/`getInt`/`getLong`

With #1 fixed, testMultiClose progressed to
`CorruptIndexException: truncated file: length=79 but
expectedLength==5692549928996306944` — that magic number is **79
byte-reversed**. `order(LITTLE_ENDIAN)` (exactly what
`BufferedIndexInput.refill` sets on its buffer) was a complete no-op on
s2-managed buffers in real-JDK mode, for three stacked reasons:

- The `order(ByteOrder)` native decoded its argument as
  `get_field(bo, 0).as_int()` — but the REAL `ByteOrder` statics store the
  `name` String in field 0, so every real constant silently decoded to
  0 = BIG_ENDIAN.
- `s2_bb_order` read/wrote the synthetic slot 5 (`BB_ORDER`); on
  real-layout buffers that index aliases `segment`. (Load-bearing
  discovery: in real-JDK mode `alloc_concurrent_synthetic(_,
  "java/nio/ByteBuffer", 6)` resolves the REAL abstract class and
  allocates its full 11-field layout — there are no 6-slot synthetic
  buffers at all; slots 1/2/3 aligning with position/limit/capacity is
  what kept the rest of the family working.)
- Nothing seeded the named `bigEndian` field to the JDK BIG_ENDIAN default
  on ctor-bypassing allocation.

`Lucene104PostingsReader`'s `expectedDocFileLength = metaIn.readLong()`
therefore came back byteswapped. Fixed by decoding both ByteOrder
representations, discriminating the layout by PHYSICAL FIELD COUNT (6-slot
pure-synthetic → `BB_ORDER` slot; otherwise the named
`bigEndian`/`nativeByteOrder` booleans that native-io's
`dbb_allocate_direct0` already seeds), seeding the JDK default in
`bb_write_hb`, returning the real `ByteOrder` statics from `order()`
(identity comparisons), and routing slice/view order propagation through
the layout-aware accessors. Note: neither class name nor name-based field
resolution discriminates the two layouts — both succeed on both.

### Diagnostics added

- `CRATONVM_DBG_BUFUNDER=1` (`vm/src/runtime/exceptions.rs`): dumps the live
  Java stack when a `BufferUnderflowException` is raised Rust-side — these
  never pass the `Athrow` opcode, so `CRATONVM_DBG_ATHROW` only ever showed
  the later Java-level rethrow (`IOUtils.rethrowAlways`), which is what sent
  the original triage to `CodecUtil.checkFooter` instead of the true origin
  (`BufferedIndexInput.readByte` → forced-native `ByteBuffer.get()`).

### Residuals (split out, NOT this family)

- The s2 synthetic ByteBuffer family has further real-JDK direct-buffer gaps
  (`get([BII)`/`get([B)` fall back to reading from the DESTINATION array on
  a direct receiver; `equals`/`hashCode`/`compareTo`/`compact` are
  heap-array-only) — see
  `docs/known-issues/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps.md`.
- Forcing `-Dtests.directory=ByteBuffersDirectory` fails testMultiClose with
  `NoSuchMethodException: <init>` (reflective `Directory` reconstruction
  gap), and a Lucene `ChecksumIndexInput.getChecksum()` value diverges from
  HotSpot on identical file bytes (self-consistent within CratonVM, so
  same-VM write/read cycles pass; `java.util.zip.CRC32` itself verified
  correct) — both noted in the same residual doc.

Validation runs (Azure Linux host, `/data/data/esvec-probe-results/`):
`fixed-final-20260710` (fixes #1+#2 at dev `4b08ffad`: ES815 6/6 PASS, Hnsw
17/17 PASS, Flat progressed underflow→byteswap face) and
`base-validated-20260710` (all three fixes at dev `4b08ffad`: all 3 rows
PASS). Standalone probes in `/tmp/Probe{Put,ChanRead2,NIOFS,LongRT,Order,Crc}.java`.

NOTE on the validation base: the 3-class suite validation ran at dev
`4b08ffad` + these fixes, NOT at the merge-time dev tip — every
`RandomizedRunner`-based ES class at the current tip is killed at bootstrap
by the unrelated OPEN regression
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md`
(introduced by `aa21e334`, bisected by another session). The standalone
probes (which do not use RandomizedRunner) pass identically when built at
the tip with these fixes. Re-run the three classes once that regression is
fixed.


## Update 2026-07-10 (follow-up session)

Reproducing this family first required fixing an unrelated, more severe
regression: `RandomizedContext.current()` started returning `null` instead
of throwing `IllegalStateException` (introduced by dev commit `4978c5d5c`
"Speed up Elasticsearch sliced IVF native paths", which rewrote
`RandomizedContext` as a native for performance). That broke
`AssertingCodec.<init>`'s `catch (IllegalStateException e) { targetClass =
null; }` fallback for code running outside a randomized-test thread (e.g. a
test class's own `<clinit>`), turning a tolerated case into an uncaught NPE
-> `ExceptionInInitializerError` that prevented every class in this family
(and likely much of the broader ES suite) from even loading. See
`docs/internal/fixed-suite-bugs/randomizedcontext-current-null-instead-of-throw-FIXED.md`
for that fix (merged separately; required before this doc's rows could be
re-tested at all).

With that blocker fixed and reverified against the same seed
(`B17AC9D3E1F2A0C4`):
- `ES815BitFlatVectorFormatTests` — **PASS**, 5/5 repeated runs, 6/6 tests each (was the AIOOBE row).
- `ES93HnswBFloat16VectorsFormatTests` — **PASS**, 17/17 tests (was the AIOOBE row).
- `ES93FlatBFloat16VectorFormatTests` — **STILL FAILS**, deterministically, same seed, both JIT-on and `--nojit`: `testMultiClose` throws `BufferUnderflowException` with the same `Caused by: java.lang.Object` corruption.

It's unclear whether the first two rows were fixed by a side effect of the
IVF-speedup commit's native vector math rewrite, or whether they were always
seed/timing-dependent and simply didn't trigger this time — the
RandomizedContext fix is what made them *testable* again, not necessarily
what fixed them. Re-verify with a spread of seeds before fully retiring
those two rows from this family.

### Root cause of the `Caused by: java.lang.Object` corruption (testMultiClose)

Added temporary env-gated instrumentation (`CRATONVM_DBG_CAUSE=1`, left in
tree in `native-builtins/src/lang_misc.rs`'s `write_throwable_cause`/
`throwable_cause`, following the codebase's existing `CRATONVM_DBG_*`
diagnostic pattern) that logs every write and read of a `Throwable.cause`
field with the object's class and identity hash. Reproducing
`ES93FlatBFloat16VectorFormatTests.testMultiClose` with it enabled shows:

```text
CAUSE_DBG_WRITE this=java/nio/BufferUnderflowException hash=493728 cause=SELF
CAUSE_DBG_WRITE this=java/lang/reflect/InvocationTargetException hash=493747 cause=java/nio/BufferUnderflowException hash=493728
...
CAUSE_DBG_READ  this=java/nio/BufferUnderflowException hash=493728 cause=java/lang/Object cause_hash=493742
```

The `BufferUnderflowException` (identity hash `493728`) is constructed
correctly — `cause` is written as the self-referential JDK sentinel
(`cause == this`, meaning "no cause set"). No code anywhere in the run ever
calls `write_throwable_cause` with hash `493742` as a value for *any*
object's cause field — that identity never appears on a WRITE line at all.
Yet the final read of the *same* object's `cause` field (during
`printStackTrace` -> `throwable_cause`, driven by JUnit's
`Throwables.getFullStackTrace`) returns a live, valid `java.lang.Object`
instance with a *different* identity hash (`493742`), not the self-sentinel
it was constructed with.

This is not a native-logic bug (the write path is correct) and not
"corrupted/garbage bytes" either — `493742` is a real, addressable object
that decodes as a valid `Value::Object`, which is why `throwable_cause`'s
read succeeds and returns `Some(...)` instead of tripping the
`read_value_checked_atomic` corrupt-cell guard for *this* slot. (A companion
`gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)`
HIB-CV-32 diagnostic fires on a *different*, nearby slot in the same run,
suggesting broader heap disturbance around the same GC cycle rather than an
isolated one-field bug.)

The pattern — a field written as a **self-reference** later reading back as
an unrelated, live object — matches the self-forwarding class of GC bug
already tracked in this codebase (see the G1 parallel-evac self-forward UAF:
a self-referential pointer is not correctly relocated when the object itself
moves during a GC cycle, so the stale self-pointer keeps pointing at the
object's *old* address; once that address is reused by a later allocation —
here, apparently a plain `new Object()` — reading the field returns whatever
now lives there). `Throwable.cause = this` (set by
`native_exc_init_noargs`/`capture_throwable_trace`'s sibling `backtrace =
this`) is exactly this shape: a self-referential field on a moving-GC-managed
object. This differs from the already-fixed `G1_PARALLEL_EVAC`-gated bug in
that it reproduces under the **default** GC configuration (no
`CRATONVM_G1_PARALLEL_EVAC` set) and under **both** `--nojit` and JIT-on —
if it's the same class of forwarding bug, it lives in the default
mover/relocator path, not the experimental parallel evacuator.

Not yet fixed — pinning down exactly which relocator path drops the
self-forward update, and confirming the "reused address" theory (e.g. a
poisoning/canary build that fills freed regions with a recognizable pattern
before reuse), is follow-up work. Keep this doc open for that residual.

## Original entry (2026-07-10, before this update)

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Family count:
- 3 FAIL rows:
  - `server org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests` -> `ArrayIndexOutOfBoundsException`
  - `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBFloat16VectorsFormatTests` -> `ArrayIndexOutOfBoundsException`
  - `server org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat16VectorFormatTests` -> `BufferUnderflowException`

User-visible signals:
```text
java.lang.ArrayIndexOutOfBoundsException
Caused by: java.lang.Object
```

```text
java.nio.BufferUnderflowException
Caused by: java.lang.Object
```

HotSpot controls:
- `ES815BitFlatVectorFormatTests`: PASS, 3.6s, run `esprobe-hotspot-vector-815-20260710`.
- `ES93FlatBFloat16VectorFormatTests`: PASS, 4.4s, run `esprobe-hotspot-vector-bfloat-20260710`.
- `ES93HnswBFloat16VectorsFormatTests`: PASS, 4.9s, run `esprobe-hotspot-vector-hnsw-bfloat-20260710`.

Focused CratonVM throw-debug evidence:
- Run: `esprobe-throw-aioobe-20260710`
- Class: `ES815BitFlatVectorFormatTests`
- Result: FAIL, 68.438s.
- Throw site:
```text
ATHROW class=java/lang/ArrayIndexOutOfBoundsException msg="<no msg>"
  ATHROW-STK[33] org/elasticsearch/index/codec/vectors/BaseKnnBitVectorsFormatTestCase.testRandom pc=580
```

- Run: `esprobe-throw-bufunder-20260710`
- Class: `ES93FlatBFloat16VectorFormatTests`
- Result: FAIL, 16.208s.
- Throw site:
```text
ATHROW class=java/nio/BufferUnderflowException msg="<no msg>"
  ATHROW-STK[39] org/apache/lucene/codecs/CodecUtil.checkFooter pc=122
  ATHROW-STK[38] org/apache/lucene/codecs/lucene104/Lucene104PostingsReader.<init> pc=183
  ATHROW-STK[33] org/apache/lucene/tests/index/BaseIndexFileFormatTestCase.testMultiClose pc=471
```

Evidence:
- AIOOBE stdout: `C:\craton\esfull-20260710-083851\results\esprobe-throw-aioobe-20260710\jit-throw-aioobe\logs\server.org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests.out.log`
- AIOOBE stderr: `C:\craton\esfull-20260710-083851\results\esprobe-throw-aioobe-20260710\jit-throw-aioobe\logs\server.org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests.err.log`
- BufferUnderflow stdout: `C:\craton\esfull-20260710-083851\results\esprobe-throw-bufunder-20260710\jit-throw-bufunder\logs\server.org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat.3125d9cdc58a.out.log`
- BufferUnderflow stderr: `C:\craton\esfull-20260710-083851\results\esprobe-throw-bufunder-20260710\jit-throw-bufunder\logs\server.org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat.3125d9cdc58a.err.log`

Interpretation:
- These are CratonVM-only vector codec correctness failures, but the exact lower-level corruption is not yet isolated.
- The bizarre `Caused by: java.lang.Object` output is itself a VM divergence and may be obscuring the real stack/cause.
- Keep this as one residual family until the common lower-level cause is split or proven separate.

Not duplicates:
- These rows are not the `FloatBuffer.order()` no-Code family; the throw-debug rows point to Lucene vector/random codec work and footer reading rather than no-Code dispatch.
- These rows are also not the older fixed vector score/value/footer families unless a later focused probe proves the same root.
