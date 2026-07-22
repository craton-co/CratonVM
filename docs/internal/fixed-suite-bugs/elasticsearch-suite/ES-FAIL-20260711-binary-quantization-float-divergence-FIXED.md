# ES failure - binary quantization float divergence - FIXED

Status: FIXED (2026-07-12), branch `fix/esvec-osq-fma-divergence-20260712`.

## Original report

Focused probe against `dev` commit `d274d898c43a4ca07ac877ba85543d153d2ea83c`,
built as `cratonvm-es-focused-currentdev-20260711-172542`:

| VM mode | Class result |
| --- | --- |
| HotSpot | PASS, 8 tests, 0 failures |
| CratonVM JIT on | FAIL, 8 tests, 1 failure |
| CratonVM JIT off | FAIL, 8 tests, 1 failure |

Class: `org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests`.
The sole divergent test was `testQuantizeForQueryCosine`:

```text
java.lang.AssertionError: expected:<-0.086002514> but was:<-0.0860025>
```

## Root cause

`org.elasticsearch.simdvec.ESVectorUtil.dotProduct(float[], float[])` and
`.squareDistance(float[], float[])` are intercepted directly by CratonVM
native Rust reimplementations
(`native_es_vector_util_dot_product_f32`/`native_es_vector_util_square_distance_f32`,
`native-builtins/src/lib.rs`) — bypassing the real Java/Lucene bytecode
entirely, in both JIT and interpreter modes (this is why the bug reproduced
identically under `-Jit on` and `-Jit off`: neither path ever touched the
real method body).

The failing test's `lower` field comes from `BinaryQuantizer.quantizeForQuery`
→ `BQVectorUtils.norm` → `ESVectorUtil.dotProduct(vector, vector)`, then
`Math.sqrt`, then an elementwise divide and a min/max scan — no rounding or
FMA-based quantization helper is involved for this specific field, so the
divergence traced directly to `dotProduct`'s own arithmetic.

The previous native implementation used a naive sequential
`sum += a[i] * b[i]` accumulation. Decompiling the actual bundled jars used
by the suite fixture (Lucene 10.4.0, `org.apache.lucene.internal.vectorization.
PanamaVectorUtilSupport`, and ES 9.5.0-SNAPSHOT's
`org.elasticsearch.simdvec.internal.vectorization.PanamaESVectorUtilSupport`)
confirmed real HotSpot on this fixture's environment does *not* run the plain
scalar fallback: the harness passes `--add-modules=jdk.incubator.vector`, and
the captured HotSpot run log shows
`Java vector incubator API enabled; uses preferredBitSize=512` — i.e. it
executes Lucene's Panama-vectorized `dotProductBody`/`squareDistanceBody`,
which accumulates into 4 independent 16-lane (512-bit) `FloatVector`
accumulators (each lane summing every 64th element via `fma`), combines them
lane-wise (`(acc1+acc2)+(acc3+acc4)`), and reduces the resulting 16-lane
vector via `FloatVector.reduceLanes(ADD)`.

Because `dotProduct`/`squareDistance` are each called only a handful of times
per test method — nowhere near a JIT compilation threshold — the Vector API
calls (`FloatVector.fromArray`/`.fma`/`.add`/`.reduceLanes`) actually run
through their plain-Java interpreter fallback bodies, not a hardware SIMD
intrinsic. Those fallback bodies are well-defined, portable Java (see
`jdk.incubator.vector.FloatVector.rOpTemplate`/`reductionOperations`):
`reduceLanes(ADD)` is a strict sequential left-to-right fold starting at
`0.0f`. This makes the exact result reproducible in scalar Rust: it just has
to mirror the same 16-lane grouped fma accumulation and sequential final
reduction, using the JVM's actual preferred SIMD width for `float`
(`VectorSpecies.ofPreferred`), not a naive single running sum. Floating-point
addition isn't associative, so the summation order matters at the ULP level
— exactly the class of bug this was.

A second, related bug was found and fixed in the same native family while
investigating: `java_math_round_f32` (used by
`calculateOSQLoss`/`quantizeVectorWithIntervals`, also in
`native-builtins/src/lib.rs`) computed `Math.round` as naive
`floor(v + 0.5)`, which mis-rounds the largest float value just below a
half-integer boundary (the classic JDK-6430675 bug class — `v + 0.5` itself
rounds up to the next integer in IEEE-754 before the `floor` ever runs).
This codebase already has a correct, bit-exact `Math.round(float)`
implementation elsewhere (`lang_math::round_float`, used for the real
`java.lang.Math.round(F)I` native) — `java_math_round_f32` was an
independent, divergent, duplicate implementation of the same operation that
never got the same fix. It now delegates to `lang_math::round_float` instead
of duplicating the logic.

## Fix

`native-builtins/src/lib.rs`:
- `native_es_vector_util_dot_product_f32`/`es_dot_product_f32`: rewritten to
  mirror Lucene's Panama-vectorized `dotProductBody` exactly — 4 independent
  `lanes`-wide fma accumulators strided through the array, a same-width
  vector-tail fold into the first accumulator only, lane-wise combination,
  then a strict sequential reduction, before a scalar fma tail for any
  remaining elements.
- `native_es_vector_util_square_distance_f32`/`es_square_distance_f32`: same
  shape, accumulating `(a[i]-b[i])^2` per lane instead of `a[i]*b[i]`.
- `native_es_vector_util_square_distance_f32_offset`: switched from a plain
  `+=` accumulation to `mul_add` (fma), matching
  `DefaultESVectorUtilSupport.squareDistance(float[],float[],int,int)`'s own
  (never-unrolled) sequential-fma implementation.
- New `panama_preferred_lanes_f32()` helper: picks the lane count (16/8/4)
  via runtime CPU feature detection (`avx512f`/`avx2`/baseline), mirroring
  how `VectorSpecies.ofPreferred(float.class)` actually picks its width on
  real hardware — this is hardware-dependent, not a fixed constant, so the
  fix stays correct across different deployment targets rather than only
  the one Azure host it was verified on.
- `java_math_round_f32` now delegates to `lang_math::round_float`
  (`native-builtins/src/lang_math.rs`, made `pub(crate)`) instead of
  duplicating the naive, incorrect `floor(v + 0.5)` formula.

## Verification

Rebuilt as `cratonvm-esvec-osq-fma-20260712` and re-ran the exact repro
directly against the same fixture
(`/data/data/es-jit-deopt-gc-bundle-20260708-214648/elasticsearch`, Lucene
10.4.0 / ES 9.5.0-SNAPSHOT, JDK 21):

| VM mode | Class result |
| --- | --- |
| HotSpot | OK, 8 tests |
| CratonVM JIT on | OK, 8 tests |
| CratonVM JIT off | OK, 8 tests |

Also ran the 26 test classes in `all-classes.tsv` matching
`binaryquantiz|hnswbinaryquantiz|diskbbq|osqvectors` (the broader OSQ/binary
-quantization family this native touches) against the fixed binary: 8/26
pass cleanly; the other 18 all show the identical, already-documented,
unrelated `Suite timeout exceeded (>= 580000 msec)` /
`NoSuchMethodError: java/lang/StringBuilder.flush()V` signature described in
[`../internal/elasticsearch-restclient-builder-suite-timeout.md`](../internal/elasticsearch-restclient-builder-suite-timeout.md)
(a pre-existing RandomizedTesting suite-timeout-accounting bug, unrelated to
vector arithmetic — that doc explicitly calls out `BinaryQuantizationTests`
itself as historically affected by the same symptom, independent of this
fix). None of the 18 show an assertion-value mismatch; this fix's blast
radius (two summation-order-sensitive natives plus one duplicate rounding
helper) has no plausible mechanism to cause a suite-level timeout/threading
artifact, so these are not regressions from this change.
