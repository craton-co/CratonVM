# ES failure family - BFloat16 vector value becomes null

Status: FIXED (2026-07-10)

Fix summary:
- The null/vector readback failures were cleared by the same real-JDK vector storage fixes already landed for `updateDocument(Term, Iterable)J`, regex/runtime-version real-layout handling, and this branch's float-array raw-copy fix.
- The last materialization problem was Lucene vector bytes being written as zeros through `FloatBuffer.put(float[])`; fixing `Unsafe.copyMemory` float-array byte encoding made the representative BFloat16 vector classes pass cleanly.

Validation:
- `/tmp/cratonvm-ES93HnswBFloat16VectorsFormatTests-floatview-r2-1783666860`: `OK (60 tests)`.
- `/tmp/cratonvm-ES93ScalarQuantizedBFloat16VectorFormatTests-floatview-r2-1783666860`: `OK (54 tests)`.

Signal:
- `java.lang.IllegalArgumentException: vector value must not be null`

Full rerun count:
- Run: `es-nonpassed-rerun-20260708-191002`
- Direct vector-null FAIL rows: 2 of 1064 total FAIL rows.
- Classes: `ES93HnswBFloat16VectorsFormatTests`, `ES93ScalarQuantizedBFloat16VectorFormatTests`.

Representative class:
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBFloat16VectorsFormatTests`

Current-dev proof:
- Probe run: `es-faildocs-probe-20260709-073704`
- HotSpot: PASS, rc=0, 4.072s.
- CratonVM JIT: CRASH, rc=139, 5.221s, no Java-level exception captured before exit.
- CratonVM --nojit: FAIL, rc=1, 47.311s, 6 JUnit failures.

Representative --nojit failure details:
- `IllegalArgumentException: vector value must not be null`
- `CorruptIndexException: codec footer mismatch`
- `IllegalStateException: this writer hit an unrecoverable error; cannot merge`
- `ArithmeticException: / by zero`
- `NoSuchMethodError: ... ES93HnswBFloat16VectorsFormatTests.updateDocument(Term, Iterable)J`

Interpretation:
- HotSpot reaches a clean pass while CratonVM returns a null vector value in the same test fixture.
- The same no-JIT run also shows footer mismatch and `updateDocument` linkage failures, so this class sits at the intersection of multiple vector residuals.
- Track the null-vector signal separately because it points at vector value materialization rather than only scorer arithmetic.

Next investigation:
- Isolate the vector read path that converts stored BFloat16 bytes into the vector value returned to Lucene/Elasticsearch.
- Check for CratonVM-specific null returns from vector field readers, array materialization, or native/vector-provider fallback code.
