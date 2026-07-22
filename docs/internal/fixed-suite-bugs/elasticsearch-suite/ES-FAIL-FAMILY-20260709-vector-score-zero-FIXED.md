# ES failure family - vector scoring returns zero under CratonVM

Status: FIXED (2026-07-10)

Fix summary:
- The codec-format zero-score/vector-zero rows were the same float-array raw-copy bug as the codec-footer family: JDK `FloatBuffer.put(float[])` bulk-copied `float[]` source values through `Unsafe.copyMemory`, but CratonVM encoded `Value::Float` as zero.
- `native-builtins/src/lib.rs` now byte-encodes `float[]` values from `Value::Float` and reconstructs `Value::Float` on raw byte writes.
- A separate query-level residual remains open as `docs/known-issues/elasticsearch-suite/ES-FAIL-FAMILY-20260710-diversifying-children-ivfknn-docid-mismatch.md`; it no longer has the old zero-score shape.

Validation:
- `/tmp/cratonvm-ES940v1DiskBBQVectorsFormatTests-floatview-r2-1783666743`: `OK (52 tests)`.
- `/tmp/cratonvm-ES93HnswBinaryQuantizedBFloat16VectorsFormatTests-floatview-r2-1783666797`: `OK (58 tests)`.
- `/tmp/cratonvm-es93-flatvector-testRandom-floatview-r2-1783666504`: old `expected 0.074157976 but was 0.0` repro passed, `OK (2 tests)`.
- `/tmp/cratonvm-es93-flatvector-testSortedIndex-floatview-r2-1783666545`, `/tmp/cratonvm-es93-flatvector-testMismatchedFields-floatview-r2-1783666545`, `/tmp/cratonvm-es93-flatvector-testIndexedValueNotAliased-floatview-r2-1783666545`: all passed, `OK (2 tests)` each.

Signals:
- `AssertionError: expected:<1.0> but was:<0.0>`
- Similar vector assertion notes where expected non-zero scores are returned as `0.0`.
- Related vector-search assertions where the returned score/value is non-zero but wrong, for example `expected:<0.027795367> but was:<0.012313265>`.

Representative class:
- `server org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v1DiskBBQVectorsFormatTests`

Full rerun count:
- Run: `es-nonpassed-rerun-20260708-191002`
- Direct vector assertion FAIL rows: 17 of 1064 total FAIL rows.
- Rows include DiskBBQ, ES814/ES816/ES818/ES93 vector format tests, `DiversifyingChildrenIVFKnnFloatVectorQueryTests`, and `IVFKnnFloatSlicedVectorQueryTests`.

Probe results:
- HotSpot: status=PASS, rc=0, seconds=11.136, tests=52, mode=triage-vectorzero-hotspot
- CratonVM --nojit: status=FAIL, rc=1, seconds=51.780, tests=52, mode=triage-vectorzero-nojit

Interpretation:
- HotSpot passes, CratonVM `--nojit` fails with the same zero-score assertion, so this is a broader runtime/native/vector implementation issue rather than a JIT miscompile.
- The affected classes sit around ES/Lucene vector formats and native/vector access paths.


Current probe after es-fixture branch:
- `probe-es-fixture-20260708-220010-vectorzero-r6`, CratonVM --nojit, still FAIL: 52 tests, 10 failures.
- The previous FileChannelImpl.open missing-method warnings are gone after registering both JDK 21 and JDK 25 FileChannelImpl.open descriptors, plus NativeThreadSet/FileKey bridges.
- Remaining signal is unchanged vector score zero assertions plus one Lucene `CorruptIndexException` footer mismatch; keep this issue open.

Current-dev residual probe:
- Probe run: `es-faildocs-probe-20260709-073704`
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- JDK baseline: `/data/data/jdk25-real`
- Hang timeout: 600 seconds

Representative results:
- HotSpot `ES940v1DiskBBQVectorsFormatTests`: PASS, rc=0, 4.684s.
- CratonVM JIT `ES940v1DiskBBQVectorsFormatTests`: CRASH, rc=139, 6.443s, no Java-level exception captured before exit.
- CratonVM --nojit `ES940v1DiskBBQVectorsFormatTests`: FAIL, rc=1, 32.274s, `AssertionError: expected:<1.0> but was:<0.0>`.
- HotSpot `ES93HnswBinaryQuantizedBFloat16VectorsFormatTests`: PASS, rc=0, 3.652s.
- CratonVM --nojit `ES93HnswBinaryQuantizedBFloat16VectorsFormatTests`: FAIL, rc=1, 32.872s, `AssertionError: expected:<0.027795367> but was:<0.012313265>`.
- HotSpot `DiversifyingChildrenIVFKnnFloatVectorQueryTests`: PASS, rc=0, 2.420s.
- CratonVM --nojit `DiversifyingChildrenIVFKnnFloatVectorQueryTests`: FAIL, rc=1, 7.013s, 2 failures including `AssertionError` and `ComparisonFailure: expected:<[1]0> but was:<[]0>`.

Current interpretation:
- The zero-score family still reproduces on current `dev` without JIT, so it remains a runtime/vector-codec bug.
- Current JIT often exits 139 before reaching the Java assertion; treat that as a separate crash surface, not as evidence this family is fixed.
- Closely related vector residuals now have their own docs: footer/checksum corruption, null vector values, and `updateDocument` linkage failures.

## Current-dev 120s full non-passed rerun

- Run: `es-nonpassed-currentdev-20260709-082115`
- `DiversifyingChildrenIVFKnnFloatVectorQueryTests` remains one of only two Java-level FAIL rows after the current-dev rerun.
- Result: FAIL, rc=1, 6.411s.
- Note: `java.lang.AssertionError`.
- Other vector-score representatives mostly crash rc=139 before Java-level assertion reporting, so this row keeps the vector assertion family open but no longer gives the broader old 17-row FAIL spread on current `dev`.
