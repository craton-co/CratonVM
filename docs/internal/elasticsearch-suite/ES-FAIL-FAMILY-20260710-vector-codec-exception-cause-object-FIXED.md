# ES FAIL family - vector codec exceptions with corrupted Throwable cause output

Status: OPEN (narrowed — 2 of 3 rows now PASS; 1 residual root-caused, not yet fixed)

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
