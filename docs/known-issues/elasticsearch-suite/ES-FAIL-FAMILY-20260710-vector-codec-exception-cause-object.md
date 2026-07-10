# ES FAIL family - vector codec exceptions with corrupted Throwable cause output

Status: OPEN

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
