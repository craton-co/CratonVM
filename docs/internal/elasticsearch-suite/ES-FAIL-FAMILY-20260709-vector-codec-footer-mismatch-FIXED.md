# ES failure family - vector codec footer/checksum corruption

Status: FIXED (2026-07-10)

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
