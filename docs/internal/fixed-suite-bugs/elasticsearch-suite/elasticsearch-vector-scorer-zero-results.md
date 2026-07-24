# Elasticsearch vector scorers return zero or wrong scores

Status: fixed

Date observed: 2026-07-02

Date fixed: 2026-07-04

## Summary

Several vector codec and vector query tests produce incorrect scores or vector
values under CratonVM. HotSpot passes the same representative classes.

Common signatures:

```text
java.lang.AssertionError: expected:<1.0> but was:<0.0>
java.lang.AssertionError: expected:<0.3569802> but was:<0.0>
java.lang.AssertionError: expected:<0.5> but was:<0.071428575>
```

Many of the same classes also hit the `SegmentVarHandle` foreign-memory issue,
but the zero-score assertions are tracked separately because some failures are
pure result mismatches.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 14 CratonVM-only failures containing vector score or
value mismatches.

Representative row:

```text
index=1427
module=server
class=org.elasticsearch.index.codec.vectors.es93.ES93HnswVectorsFormatTests
CratonVM=FAIL, 271.248s
HotSpot=PASS, 63.802s
```

Other examples:

```text
org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat16VectorFormatTests
org.elasticsearch.index.codec.vectors.es94.ES94HnswScalarQuantizedVectorsFormatTests
org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests
org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatVectorQueryTests
```

## Update 2026-07-03 — confirmed independent of SegmentVarHandle (now fixed)

`SegmentVarHandle.get/set` (see
[elasticsearch-vector-segmentvarhandle-memorysegment.md](elasticsearch-vector-segmentvarhandle-memorysegment.md),
now fixed on `dev` — both the original `NoSuchMethodError` and a follow-up
off-by-16 heap-addressing bug found while verifying it) was previously
masking this bug on several classes. With it fully fixed,
`ES818BinaryQuantizedVectorsFormatTests` runs far enough to hit these same
zero-score assertions directly:

```text
1) testRescoreUsesRawVectorSlice(...ES818BinaryQuantizedVectorsFormatTests)
java.lang.AssertionError: expected:<0.7245078> but was:<0.0>
2) testMismatchedFields(...)
java.lang.AssertionError: expected:<1.0> but was:<0.0>
4) testSortedIndex(...)
java.lang.AssertionError: expected:<-1.0> but was:<0.0>
5) testAddIndexesDirectory01(...)
java.lang.AssertionError: expected:<1.0> but was:<0.0>
```

All four expect *different* nonzero values but all get exactly `0.0` —
consistent with the scoring computation returning a hard zero (e.g. a
defensive early-return or null/uninitialized quantization state) rather than
reading corrupted/random vector bytes (which would more likely produce
random nonzero garbage). The `SegmentVarHandle` memory-access arithmetic was
separately verified correct against real JDK 25 across off-heap `Arena`,
heap `MemorySegment.ofArray`, sliced-heap, and real memory-mapped-file
segments — so this zero-score bug is confirmed to live elsewhere (likely
BBQ/binary-quantization dot-product or quantization-state setup), not in the
segment memory-access path.

Two more failures in the same run look unrelated to scoring:
`testKnnVectorFieldMissingFromOneSegment` →
`IllegalArgumentException: Not a supported array class: byte[]`, and
`testMultiClose` → `FileAlreadyExistsException`.

This run also separately hit the Lucene randomizedtesting framework's own
internal ~580s suite timeout (this class takes ~610s under CratonVM JIT-on)
— a residual interpreter-performance characteristic, not a correctness
issue; see the performance note in the internal `SegmentVarHandle` doc
linked above.

Evidence:
`C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-segvh-verify-es818-20260703\all-jit\logs\server.org.elasticsearch.index.codec.vectors.es818.ES818BinaryQu.29ee1bcc736c.out.log`

## Resolution 2026-07-04

Root cause: the JDK Vector API bridge stored each synthetic vector as one
summary integer (`data_hash`) instead of preserving per-lane values. Vector
loads from primitive arrays, lane-wise arithmetic, FMA, and reductions therefore
computed from a deterministic hash surrogate rather than the actual vector
payload. Dot-product style scorer code could collapse to exact zero or produce
wrong fractional scores even when the underlying vector bytes were correct.

Fix: `../../../../native-builtins/src/vector_api.rs` now stores vector lanes in a synthetic
`long[]` payload, preserving raw integer lanes and raw float/double bit
patterns. `fromArray`, broadcast, arithmetic, unary ops, FMA, reductions,
lane access, `withLane`, `toArray`, `intoArray`, compare masks, blend, and
rearrange now operate on the lane payload. `VectorMask` also carries exact lane
bits so compare/blend no longer loses lane positions.

Regression coverage:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-es-vector-scorer-zero-results-20260704'
cargo test -p cratonvm-native-builtins vector_api --lib
```

Result: `58 passed`; includes
`test_float_vector_mul_reduce_uses_real_lanes`, which covers
`fromArray -> mul -> reduceLanes(ADD)` and would have failed under the old
hash-summary model.

Additional check:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-es-vector-scorer-zero-results-20260704'
cargo check -p cratonvm-native-builtins
```

Result: passed.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1427 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-vector-score-zero-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.es93.ES93HnswVectorsFormatTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests.out.log
```
