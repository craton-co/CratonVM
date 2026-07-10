# ES FAIL family - FloatBuffer abstract receiver no-Code in vector codecs

Status: OPEN

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Family count:
- 22 rows in the stopped partial run.
- 20 FAIL rows in vector codec classes.
- 2 HANG rows whose logs include related `FloatBuffer.get(I)F` no-Code signals.

Primary signal:
```text
java.lang.AbstractMethodError: method java/nio/FloatBuffer.order()Ljava/nio/ByteOrder; has no Code attribute
```

Focused CratonVM proof:
- Run: `esprobe-nocode-floatbuffer-20260710`
- Class: `server org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v1DiskBBQVectorsFormatTests`
- Result: FAIL, 23.213s, 11 failures.
- With `CRATONVM_DBG_NOCODE=1`, stderr records:
```text
[DBG_NOCODE] method java/nio/FloatBuffer.order()Ljava/nio/ByteOrder; has no Code attribute | recv_cid=2478 recv_class=java/nio/FloatBuffer
```

HotSpot control:
- Run: `esprobe-hotspot-floatbuffer-20260710`
- Same class: PASS, 4.8s.

Affected rows observed in the stopped partial run:
- `ES813FlatVectorFormatTests`
- `ES813Int8FlatVectorFormatTests`
- `ES814HnswScalarQuantizedVectorsFormatTests`
- `ESNextOversamplingMetaTests`
- `PreconditionerTests`
- `ES940v1DiskBBQVectorsFormatTests`
- `ES940v2DiskBBQVectorsFormatTests`
- `ESNextDiskBBQVectorsFormatTests`
- `ES816BinaryQuantizedVectorsFormatTests`
- `ES816HnswBinaryQuantizedVectorsFormatTests`
- `ES818BinaryQuantizedVectorsFormatTests`
- `ES818HnswBinaryQuantizedVectorsFormatTests`
- `ES93BinaryQuantizedBFloat16VectorsFormatTests`
- `ES93BinaryQuantizedVectorsFormatTests`
- `ES93FlatVectorFormatTests`
- `ES93HnswBinaryQuantizedBFloat16VectorsFormatTests`
- `ES93HnswBinaryQuantizedVectorsFormatTests`
- `ES93HnswScalarQuantizedVectorsFormatTests`
- `ES93HnswVectorsFormatTests`
- `ES93ScalarQuantizedVectorsFormatTests`
- HANG: `ES93HnswScalarQuantizedBFloat16VectorsFormatTests`
- HANG: `ES93ScalarQuantizedBFloat16VectorFormatTests`

Evidence:
- Craton stdout: `C:\craton\esfull-20260710-083851\results\esprobe-nocode-floatbuffer-20260710\jit-floatbuffer\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v.0a9b9e987125.out.log`
- Craton stderr: `C:\craton\esfull-20260710-083851\results\esprobe-nocode-floatbuffer-20260710\jit-floatbuffer\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v.0a9b9e987125.err.log`
- HotSpot result: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-floatbuffer-20260710\hotspot-floatbuffer\results.tsv`

Interpretation:
- CratonVM is dispatching against a receiver whose runtime class is the abstract `java/nio/FloatBuffer`, so calls such as `order()`, `get(int)`, and `put(int,float)` can land on methods with no Code attribute.
- A tiny standalone `FloatBuffer.allocate()` / `ByteBuffer.allocateDirect().asFloatBuffer()` probe did not reproduce the no-Code error, so the failing allocation or receiver stamping appears specific to the Lucene/Elasticsearch vector path.

Not duplicates:
- This is distinct from the older fixed JDK NIO no-Code document, which covered `FileSystemProvider.getFileStore`, `Buffer.isReadOnly`, and `CharBuffer` gaps.
- Keep this as one family doc; do not add per-class vector docs for the same `FloatBuffer` no-Code signature.
