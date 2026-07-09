# ES CRASH family - current-dev fail-probe representatives exit 139

Status: OPEN

Source:
- Probe run: `es-faildocs-probe-20260709-073704`
- Trigger: representative rerun of old FAIL rows after the large `findNative` and `SymbolLookup.find` families were fixed on `dev`.
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- Mode: CratonVM JIT on.
- Hang timeout: 600 seconds.

Exact count:
- Selected representatives: 14.
- CratonVM JIT PASS: 1.
- CratonVM JIT FAIL: 2.
- CratonVM JIT CRASH: 11, all rc=139.

Crash rows:
- `libs/cli-terminal org.elasticsearch.cli.terminal.JsonTerminalTests` -> rc=139, note includes `MemoryLayout.varHandle` AbstractMethodError.
- `server org.elasticsearch.index.codec.postings.ES812PostingsFormatTests` -> rc=139, stderr shows out-of-bounds field reads on `java/lang/foreign/SymbolLookup` and `java/lang/invoke/MethodHandle` before exit.
- `server org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v1DiskBBQVectorsFormatTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.index.codec.zstd.Zstd814BestCompressionStoredFieldsFormatTests` -> rc=139, stdout includes `MemoryLayout.varHandle` AbstractMethodError.
- `server org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQBFloat16VectorsFormatTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBFloat16VectorsFormatTests` -> rc=139, no Java-level exception captured.
- `client/rest org.elasticsearch.client.RestClientSingleHostIntegTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.lucene.queries.FloatRandomBinaryDocValuesRangeQueryTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswScalarQuantizedBFloat16VectorsFormatTests` -> rc=139, stderr logs `updateDocument(Term, Iterable)J` NoSuchMethodError before exit.
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBinaryQuantizedBFloat16VectorsFormatTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.search.vectors.IVFKnnFloatSlicedVectorQueryTests` -> rc=139, no Java-level exception captured.

Baselines:
- HotSpot passed 13 of the same 14 representatives. The only HotSpot FAIL was the Zstd fixture-native-library row, not an rc=139 crash.
- CratonVM --nojit converted many of the same classes into Java-level FAIL or 600s HANG rows, which are documented separately.

Interpretation:
- This is not a full-suite crash count; it is a current-dev residual probe count from old FAIL rows.
- The rc=139 behavior often hides the Java-level residual that is visible under `--nojit`, so use this doc to track the JIT/runtime crash surface and use the fail-family docs for cleaner root-cause signals.
- The `ES812PostingsFormatTests` stderr guard warnings suggest at least one crash path still touches foreign API method-handle/SymbolLookup layout handling.

## Full non-passed rerun update

- Run: `es-nonpassed-currentdev-20260709-082115`
- Binary: `/data/data/cratonvm-targets/es-rerun-currentdev-20260709-082115/release/cratonvm-es-rerun-currentdev-20260709-082115`
- Class list: 2649 non-passed rows from the prior ES selection.
- Timeout: 120 seconds.
- Shards: 4.
- Result: 2640 CRASH, 2 FAIL, 7 PASS, 0 HANG.
- All crash rows exited rc=139.
- 2583 crash result notes directly contain `MemoryLayout.varHandle`; 2585 crash logs contain the same marker.
- 50 crash rows had blank result notes, including 8 with CratonVM GC guard out-of-bounds field markers in captured logs.

Old-HANG rerun update:
- Run: `es-hung10-currentdev-20260709-082115`
- Timeout: 1500 seconds.
- Result: 10 CRASH, 0 HANG.
- All ten old-HANG classes now exit rc=139 before the long timeout matters.
