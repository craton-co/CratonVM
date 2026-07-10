# ES failure family - DiversifyingChildren IVFKnn doc-id mismatch

Status: OPEN

Signal:
- `org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatVectorQueryTests` still fails under CratonVM `--nojit` after the vector codec zero-byte fix.
- Failure shape is no longer zero scores or zero vector values. The current assertions are small doc-id/count mismatches:
  - `testSkewedIndex`: expected `10128903`, got `10129027`
  - `testFilterWithNoVectorMatches`: expected `15683751`, got `15683875`
  - `testEmptyIndex`: expected `18059531`, got `18059655`

Current proof:
- Binary: `/data/data/bin/cratonvm-es-suite-bytebuffer-floatview-20260710-063500-r2`
- Run: `/tmp/cratonvm-DiversifyingChildrenIVFKnnFloatVectorQueryTests-floatview-r2-1783666797`
- Command shape: JUnitCore with `--nojit`, `-Dtests.seed=783661625B8D4D10`, `en-US`, `UTC`, `tests.asserts=false`.
- Result: FAIL, rc=1, 6 tests run, 3 failures.

Nearby fixed signal:
- The old codec/vector-zero rows are fixed by the float-array raw-copy change in `native-builtins/src/lib.rs`:
  - `ES940v1DiskBBQVectorsFormatTests`: `OK (52 tests)`
  - `ES93HnswBinaryQuantizedBFloat16VectorsFormatTests`: `OK (58 tests)`
  - `ES93FlatVectorFormatTests`: `OK (106 tests)`

Next investigation:
- Compare HotSpot vs CratonVM for the three named methods with per-hit/top-doc tracing.
- Focus on query result ordering, child diversification, bitset/filter iteration, or doc-id arithmetic rather than vector byte storage.
