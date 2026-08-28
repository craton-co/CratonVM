# ✅ FIXED — `GaussNewtonOptimizerWith*Test.testMaxEvaluations`: `Math.hypot` was one ULP off HotSpot

## Status
**RESOLVED 2026-08-16** on branch `fix/commonsmath-gaussnewton-maxeval-20260816`.
All four `GaussNewtonOptimizerWith{Cholesky,LU,QR,SVD}Test` classes pass, and so
does the residual this doc asked to rule in or out
(`LevenbergMarquardtOptimizerTest.testControlParameters`) — same root cause.

Filed originally as: `GaussNewtonOptimizerWith*Test.testMaxEvaluations` —
optimizer doesn't throw when max evaluations is exceeded.

## The failure, as filed

```
Failures (1):
  JUnit Vintage:GaussNewtonOptimizerWithCholeskyTest:testMaxEvaluations
    => java.lang.AssertionError: Expected Exception from: GaussNewtonOptimizer{decomposition=CHOLESKY}
```

Identical in all four decomposition variants. The test configures a tiny
evaluation budget and expects `TooManyEvaluationsException`; under CratonVM
`optimize()` returned normally instead.

## Root cause

**`Math.hypot` was backed by platform libm, and HotSpot's `Math.hypot` is
fdlibm.** One ULP of difference, in one of five residuals, changed the
optimizer's trajectory into an exact fixed point — and a fixed point is
convergence, so the evaluation budget was never reached.

The original doc's hypothesis — "almost certainly in `GaussNewtonOptimizer`'s
own evaluation-count bookkeeping/limit check" — was wrong. The counter was
fine; the optimizer genuinely converged. Both of the doc's stated alternatives
("the counter never reaches the max" vs "the throw condition doesn't fire")
described the *counter*; the actual answer was upstream of it, in arithmetic.

### How it was found

`GnProbe` reran the test's own setup and logged every evaluation. HotSpot
oscillates forever between two adjacent representations and hits the
100-evaluation ceiling; CratonVM settled and returned at evaluation 9:

```
HotSpot  eval#6 p=[405804db9476fe96, 4048114d2e7bcb72]   ... eval#99, then THREW
CratonVM eval#6 p=[405804db9476fe95, 4048114d2e7bcb70]   ... eval#9,  RETURNED
```

Evaluations 1-5 are bit-identical on both VMs, so the divergence is introduced
by the step computed *from* evaluation 5. `GnProbe2` pinned that step's
arithmetic operation by operation and found the model function itself already
disagreed — before any linear algebra. `GnProbe3` then bisected the model down
to primitives:

```
i=4  dx*dx=40a4617edca6a4b0 dy*dy=40a2a78b2b6e80b1 sum=40b38485040a92b0
     sqrt=4051abe877782c30        <- identical on both VMs
     HotSpot  hypot=4051abe877782c31
     CratonVM hypot=4051abe877782c30
```

`CircleVectorial`'s model measures point-to-centre distance with
`Vector2D.distance`, which is commons-geometry's `Math.hypot`. Every operand,
`sqrt` included, agreed; `hypot` alone did not.

### Why `Math.hypot` is fdlibm on the oracle

In JDK 25 every `java.lang.Math` transcendental is a one-line delegation —
`Math.hypot(x, y)` is literally `return StrictMath.hypot(x, y);`. `Math.f` and
`StrictMath.f` are therefore the *same function* on HotSpot except where
HotSpot substitutes an intrinsic, and HotSpot has no `hypot` intrinsic.

CratonVM's `lang_math.rs` split the surface two ways — `StrictMath` on the
in-tree fdlibm port, `Math` on platform libm — on the reasoning that libm
already satisfies `Math`'s 1-ULP specification. True of the spec, false of the
oracle: a 1-ULP-conformant answer that is not HotSpot's answer is still a
differential divergence, and here it cost four test classes.

## The fix

`native-builtins/src/lang_math.rs`: a three-way split replaces the two-way one.
`probes/MathCensus.java` replayed a HotSpot JDK 25 oracle (5400 inputs per
function, six generators from `[-1,1]` through raw bit patterns) against both
candidate backings. Rows are disagreements with HotSpot's `Math.f`, out of 5400:

| function | libm | fdlibm |   | function | libm | fdlibm |
| --- | --- | --- | --- | --- | --- | --- |
| `asin`  |  52 | **0** | | `sin`   |   9 | 134 |
| `acos`  |  90 | **0** | | `cos`   |   9 | 137 |
| `atan`  |  65 | **0** | | `tan`   |  20 | 158 |
| `atan2` | 884 | **0** | | `exp`   |   4 | 181 |
| `hypot` | 405 | **0** | | `log`   |   0 |  78 |
| `sinh`  |  77 | **0** | | `log10` | 116 | 125 |
| `cosh`  | 128 | **0** | | `pow`   |   1 |  89 |
| `expm1` |   1 | **0** | | `cbrt`  |  11 | 435 |
| `log1p` |   0 | **0** | | `tanh`  | 543 | 543 |

The left column is exactly the set HotSpot does not intrinsify, and for every
one of them fdlibm is not merely closer but **exact** — so those rows are now
registered once and shared by both classes. The right column is HotSpot's
intrinsic set (`_dsin`, `_dcos`, `_dtan`, `_dexp`, `_dlog`, `_dlog10`, `_dpow`,
`_dcbrt`, `_dtanh`), whose answers come from Intel LIBM assembly and match
neither candidate; there libm is one to two orders of magnitude closer, so those
stay split and `Math` keeps libm.

The same census found a second, unrelated defect and it is fixed in the same
change: **`Math.max`/`Math.min` did not propagate NaN**, because they were
`f64::max`/`f64::min`, whose IEEE-754-2019 `maxNum` semantics deliberately
*ignore* a NaN operand. Signed zero was unpinned for the same reason. Both
overload pairs (double and float) now transcribe `java.lang.Math`'s own bodies.

### The second census pass

`probes/MathCensus2.java` covers what the first pass does not reach — the float
overloads and the exactly-specified bit-level rows, where a divergence is a
plain bug rather than a last-ULP question. It named four more things:

* **`Math.signum`** returned a fresh canonical NaN. The JDK returns the
  *argument* (`(d == 0.0 || isNaN(d)) ? d : copySign(1.0, d)`), so sign and
  payload survive. Fixed, both overloads.
* **`Math.ulp`** did the same. The JDK's NaN/infinity arm is a single
  `Math.abs(d)`, which is why HotSpot answers `ulp(0xffc8ae0a)` with
  `0x7fc8ae0a`. Fixed, both overloads.
* **`StrictMath.copySign`** was registered to `Math.copySign`'s body. These two
  genuinely differ *by specification*: `StrictMath.copySign` is
  `Math.copySign(magnitude, isNaN(sign) ? 1.0 : sign)` — it reads a NaN sign
  argument as **positive**. Sharing one body returned a negative magnitude for
  12 of 6000 sampled pairs where HotSpot returns a positive one. Note the
  direction: here the *strict* class is the looser of the two, which is exactly
  why sharing looked safe. Fixed by splitting the registration.
* **`Math.fma`** returns a canonical NaN where HotSpot returns the NaN operand.
  **Not a defect and not fixed:** JDK 25's `Math.fma` opens with
  `if (isNaN(a) || isNaN(b) || isNaN(c)) return Double.NaN;`, so CratonVM is
  running the JDK's own body faithfully and HotSpot's `_fmaD` intrinsic is the
  one that deviates from it. Same category as `sin`/`cos`/`exp` below.

`Math.scalb(float, int)` also disagreed, on 8 of 6000 — and that one turned out
not to be a `Math` problem at all. It is
`(float)((double) f * 2^k)`, and the intermediate double passes through a
`CompactValue` slot, whose NaN-boxed encoding flattens exactly the negative
quiet NaNs with mantissa bit 50 set. Root-caused and filed separately as
`nan-payloads-lost-to-the-compactvalue-tag-collision-FIXED-20260828`; it is
payload-only, predates this change, and is unaffected by it.

## Verification

Fixed binary `/data/vm-gnopt-20260816.bin`, Azure host 2, against the same
classpath and harness as the original repro.

| class | before | after |
| --- | --- | --- |
| `GaussNewtonOptimizerWithCholeskyTest` | 1 failed | **0 failed** (21 ok) |
| `GaussNewtonOptimizerWithLUTest` | 1 failed | **0 failed** (21 ok) |
| `GaussNewtonOptimizerWithQRTest` | 1 failed | **0 failed** (21 ok) |
| `GaussNewtonOptimizerWithSVDTest` | 1 failed | **0 failed** (21 ok) |
| `LevenbergMarquardtOptimizerTest` | `testControlParameters` | **0 failed** (25 ok) |
| `StatUtilsTest` | `testMax`, `testMin` | **0 failed** (18 ok) |
| `MiniBatchKMeansClustererTest` | `testCompareToKMeans` | **0 failed** |
| `SimplexOptimizerMultiDirectionalTest` | 2 failed | 1 failed |

* `GnProbe CHOLESKY` now reports `THREW ... TooManyEvaluationsException`.
* `MathCensus check` drops from 18 divergent functions to 8 — precisely
  HotSpot's intrinsic set, plus nothing.
* The four GaussNewton classes and `StatUtilsTest` also pass with the **JIT
  enabled**, not only under `--nojit`; the JIT has no intrinsic for any of these
  functions, so it reaches the same natives.

### The whole suite

All 310 classes of `commons-math-legacy`, same runner and per-class timeout as
the run that found the bug, diffed against that run:

**Ten classes moved not-PASS → PASS**: the four `GaussNewtonOptimizerWith*Test`,
`LevenbergMarquardtOptimizerTest`, `StatUtilsTest`,
`MiniBatchKMeansClustererTest`, `CalinskiHarabaszTest`,
`SimplexOptimizerMultiDirectionalTest`, `SimplexOptimizerNelderMeadTest`. (The
last two also fail on HotSpot, so treat them as noise moving the right way
rather than as fixes.)

**No class regressed.** Two rows read PASS → HANG in the raw diff and both are
the 180-second timeout, not the change. In the baseline they had passed at
**167 s** and **143 s** — within 13 and 37 seconds of the limit — and the host
was carrying other agents' load. Re-run individually with a 900-second timeout,
ABBA-interleaved against the pre-fix binary:

```
LegendreHighPrecisionParametricTest   FIX 142s  PRE 142s  PRE 180s  FIX 157s   30 ok, 0 fail
FieldBracketingNthOrderBrentSolver    FIX 160s  PRE 161s  PRE 168s  FIX 210s    4 ok, 0 fail
```

Every arm passes, and the *pristine* binary was the slower one in two of the
four pairs — at 180 s and 210 s it would have tripped the same timeout. The
remaining non-PASS rows are unchanged from the baseline in both membership and
failure counts.

The doc's second next-step — "check whether this shares a cause with the
`LevenbergMarquardtOptimizerTest.testControlParameters` failure" — is answered
**yes**. That test builds the same `CircleVectorial` from the same five points
and the same start `{98.680, 47.345}`, so it depended on the same `hypot`.

### Throughput

The rewired rows are not slower. ABBA-interleaved against the pre-fix binary,
ns per call, JIT on:

```
FIX  hypot=101.9  atan2=89.4  cosh=99.0  sin=69.1
PRE  hypot=106.2  atan2=92.8  cosh=92.8  sin=71.5
PRE  hypot=105.3  atan2=99.9  cosh=92.5  sin=79.1
FIX  hypot= 98.1  atan2=86.6  cosh=95.3  sin=66.3
```

At ~100 ns per call the native-call overhead dominates the body either way, so
the "libm is faster for the overwhelmingly more common caller" half of the old
rationale does not survive measurement either. `sin` is the control: it is
unchanged by this fix and moves as much as the rewired rows do.

### Regression cover

Three tests in **`native-builtins/src/lang_math.rs`** — deliberately not in
`vm/src/vm/tests.rs`, which is `#[cfg(all(test, feature = "synthetic-jdk"))]`
and which CI only *compiles*, never runs. `cargo test -p
cratonvm-native-builtins --lib` is a blocking gate, so these execute:

* `math_hypot_is_registered_to_fdlibm_not_naive_sqrt` — the exact operand pair
  from this bug, asserted against HotSpot's captured answer for both classes,
  with the naive-`sqrt` value asserted separately so the test names *which*
  wrong answer it guards against.
* `math_and_strictmath_agree_on_non_intrinsified_rows` — the rule itself, not a
  sample of it: any future re-pointing of one of these rows at libm fails here
  rather than in three app suites a week later.
* `math_max_min_propagate_nan_and_pin_signed_zero`.

All three assert on the **registry**, not on the Rust helpers. The bug was never
a wrong body — `fdlibm::hypot` was correct and present the whole time — it was a
wrong *registration*, and a test that calls the helper it wants would have
passed throughout.

One edit was still required in `vm/src/vm/tests.rs`: `math_sinh_cosh_tanh` was
*asserting the bug*, comparing `Math.sinh(1.0)` against `f64::sinh` under a
comment claiming `Math.sinh` "must not" be the fdlibm body. The pre-existing
`math_max_double_with_nan` had the same shape in miniature — its comment
described the `f64::max(1.0, NaN)` divergence exactly, next to a call passing
NaN twice, which cannot observe it. Both comments now point at the tests that
do cover the case.

## Residuals (deliberately not fixed)

**HotSpot's intrinsic set still diverges by ≤2 ULP** — `sin` 9/5400, `cos` 9,
`tan` 20, `exp` 4, `log10` 116, `cbrt` 11, `pow` 1, `tanh` 543 (max 2 ULP).
These cannot be closed by choosing a backing: HotSpot's answers come from Intel
LIBM assembly kernels that agree with neither fdlibm nor the host libm, and the
host libm is already the closer of the two by a wide margin. Closing them means
porting those kernels. Nothing in the commons-math suite currently fails on
them. The full table lives in the source, above `let strict` in
`native-builtins/src/lang_math.rs`, so it stays next to the decision it
justifies.

**Not this bug, still open:** `RealVectorTest` (2), `SparseRealVectorTest` (3)
and `KendallsCorrelationTest` (4) are unchanged by this fix — same failure
counts before and after — and remain under the `commons-math` NaN-comparison
cluster doc in `docs/known-issues/`. `StatUtilsTest` was listed there too and is
now closed by the `Math.max`/`min` half of this change; that doc has been
updated to say so.

**`LevenbergMarquardtOptimizerTest.testCircleFitting2` flakes on the JIT path**
in both the fixed and the pristine binary (2 of 3 runs each, ABBA-interleaved),
and passes under `--nojit` in both. Pre-existing and unrelated; filed separately.

## Repro (historical)

```bash
cd apps/commons-math/commons-math-legacy
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>" org.junit.platform.console.ConsoleLauncher \
  --select-class org.apache.commons.math4.legacy.fitting.leastsquares.GaussNewtonOptimizerWithCholeskyTest
```

Minimal, suite-free form:

```java
double x = Double.longBitsToDouble(0xc04989b7291d9512L);
double y = Double.longBitsToDouble(0x40486eb2d16f96cbL);
Math.hypot(x, y);   // HotSpot 0x4051abe877782c31, CratonVM was ...c30
```
