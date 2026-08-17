# ✅ CLOSED — the commons-math "iterative-numeric FP divergence" cluster: every class in it is an unseeded-RNG test, and the hunt for the residual found a real `Math.pow` defect

## Status

**CLOSED 2026-08-17** on branch `fix/commonsmath-fp-residuals-20260816`.

Two separate outcomes, and they should not be conflated:

1. **The cluster itself is not a CratonVM defect.** All six classes named in the
   filing draw their input from an *unseeded* commons-rng generator, and every
   one of them fails on real HotSpot JDK 25 at the same rate. The residual the
   previous pass left open — "3 failures against HotSpot's 1, and 43 against 36"
   — does not survive an interleaved measurement.
2. **The investigation found and fixed a genuine, specification-violating
   `java.lang.Math.pow` defect** that the existing censuses were structurally
   blind to. It was not what moved these tests, but it is real, it is live in
   both dispatch modes, and it is fixed here.

## Part 1 — the cluster

### Every class in it is randomised

| class | its generator | seeded? |
| --- | --- | --- |
| `SimplexOptimizerNelderMeadTest` | `OptimTestUtils.rng()` = `ThreadLocalRandomSource.current(MWC_256)` | no |
| `SimplexOptimizerTest` | same, plus `RandomSource.KISS.create()` for simulated annealing | no |
| `SimplexOptimizerMultiDirectionalTest` | same | no |
| `MiniBatchKMeansClustererTest` | `RandomSource.MT_64.create()` | no |
| `CalinskiHarabaszTest` | `RandomSource.MT_64.create()` | no |
| `CorrelatedVectorFactoryTest` | `RandomSource.{KISS,WELL_1024_A}.create()` | no |
| `FastSineTransformerTest` | `RandomSource.MWC_256.create()` | no |

`OptimTestUtils.point(double[] value, double jitter)` perturbs the start point by
a `ContinuousUniformSampler` draw, so `testFourExtremaMinimize1` does not start
at `{-3, 0}`; it starts somewhere within `±0.1` of it, differently every run.
The evaluation-count assertions those tests carry (`nEval < 105`, `< 110`) sit
close enough to the typical count that the outcome is a coin flip.

The seeding itself is healthy on CratonVM — 200 freshly constructed unseeded
generators of each of `MWC_256`, `KISS` and `XO_SHI_RO_256_PP` gave 200 distinct
first outputs with mean popcount 31.7-32.3 and a flat first-`nextDouble`
histogram, matching HotSpot. The randomness is real on both sides; that is
exactly the problem.

### Measured failure rates, arms interleaved

Azure host 2, JDK 25, identical classpath, `HS CVM CVM HS` per class per round so
that host load (20-45 throughout, from sibling agents) is shared rather than
assigned. CratonVM on `--nojit`.

**`SimplexOptimizerNelderMeadTest`** — 7 tests, 12 runs per arm, failures per run:

```
HotSpot   1 0 0 2 0 1 2 1 1 2 2 1     total 13
CratonVM  0 1 0 1 2 0 2 0 1 1 0 2     total 10
```

The filing recorded 3-vs-1. Over 12 runs CratonVM fails **fewer** of them.

**`SimplexOptimizerTest`** — 102 parameterized tests, 11 runs per arm:

```
HotSpot   35 32 29 33 48 37 46 34 33 37 29    mean 35.7, range 29-48
CratonVM  43 39 40 38 43 41 38 38 36 38 33    mean 38.8, range 33-43
```

The previously-open "43 against 36" is one sample from each of those two
columns. CratonVM's entire observed range lies inside HotSpot's, the means
differ by 3.1 with a combined standard error of 2.1, and the deterministic
replica below shows why the remaining spread exists.

**The rest of the cluster** — 18 runs per arm, runs containing at least one
failure:

| class | HotSpot | CratonVM |
| --- | --- | --- |
| `CorrelatedVectorFactoryTest` | 7 / 18 | 7 / 18 |
| `FastSineTransformerTest` | **18 / 18** | **18 / 18** |
| `MiniBatchKMeansClustererTest` | 2 / 18 | 2 / 18 |
| `CalinskiHarabaszTest` | 10 / 18 | 8 / 18 |
| `SimplexOptimizerMultiDirectionalTest` | 12 / 17 | 13 / 17 |

`FastSineTransformerTest` — the filing's headline "directly-measured ULP-level
divergence" — fails on **every single HotSpot run**. The earlier pass had it at
4 in 10; on this host it is 18 in 18 on both VMs.

### The deterministic replica: where the numbers come from

Rates bound a claim; they do not explain it. Three probes remove the RNG and
compare bit patterns instead.

**`SimplexProbe`** replays the Nelder-Mead `FourExtrema` optimisation — the
function behind all three failing tests in `SimplexOptimizerNelderMeadTest` —
from 40 fixed start points and initial simplexes, logging every objective
evaluation as raw `doubleToRawLongBits`. HotSpot, CratonVM `--nojit`, and three
separate CratonVM JIT runs all produce the **same 4,068 lines**. No diff.

**`SoProbe`** replays all 102 rows of `SimplexOptimizerTest`'s three CSV inputs
with a seeded start point and a seeded annealer. Result:

* the 40 `nelder_mead` rows and the 32 `multidirectional` rows are
  **bit-identical** across HotSpot, CratonVM `--nojit` and CratonVM JIT —
  including every row driven by `POWELL`, `ELLI`, `SUM_POW`, `ACKLEY`,
  `RASTRIGIN`, `GRIEWANK`, `LEVY`, `SCHWEFEL`, `PERM` and `STYBLINSKI_TANG`;
* all 30 `hedar_fukushima` rows differ — **on every pairing, including each arm
  against itself**, because `HedarFukushimaTransform`'s convenience constructor
  is `this(sigma, RandomSource.KISS.create())`. The unseeded generator is inside
  the *library*, not the test, so no amount of seeding on the test side removes
  it.

That is the whole shape of the `SimplexOptimizerTest` spread: roughly a third of
the class is a coin flip that neither VM controls, and the other two thirds are
bit-for-bit equal.

**`LmProbe`**, from the sibling `testCircleFitting2` filing, is the same story
for Levenberg-Marquardt: 40 seeds, three arms, byte-identical output. See
bug-commonsmath-lm-circlefitting2-jit-flake-20260816-RETIRED.

### `AccurateMath`, not `java.lang.Math`, is what these tests actually call

Worth recording because it redirects the obvious next investigation:
`JdkMath`'s static initialiser defaults to `Impl.CM`, so
`JdkMath.atan`/`sqrt`/`pow` inside commons-math resolve to commons-math's own
`AccurateMath`, which is ordinary Java bytecode — not to `java.lang.Math` at
all. A census of all 41 of `AccurateMath`'s public scalar methods over 2,400
deterministic inputs each — 98,400 rows — reports **0 disagreements** with
HotSpot, in both `--nojit` and JIT mode. The interpreter and the JIT execute
that bytecode identically to HotSpot.

`TestFunction` (`SimplexOptimizerTest`'s objective functions) is the exception:
it calls `Math.pow`, `Math.cos`, `Math.sin`, `Math.exp` and `Math.sqrt`
directly. That is what made the `Math` residue worth pinning down, and it is
what led to Part 2.

### The `java.lang.Math` residue, fully characterised

The previous pass left this as "the remaining `Math` divergences are `sin`,
`cos`, `tan`, `exp`, `log10`, `cbrt`, `tanh` and `pow`, all ≤2 ULP … libm is
already the closer of the two by one to two orders of magnitude". Measured per
row rather than in aggregate, against a HotSpot JDK 25 oracle of 5,400 sampled
inputs per function:

| row | libm disagrees | fdlibm disagrees | closer |
| --- | --- | --- | --- |
| `sin` | 9 | 134 | libm |
| `cos` | 9 | 137 | libm |
| `tan` | 20 | 158 | libm |
| `exp` | 4 | 181 | libm |
| `log` | **0** | 78 | libm |
| `log10` | 116 | 125 | libm |
| `cbrt` | 11 | 435 | libm |
| `tanh` | 543 | 543 | tie |
| `pow` | 1 | 89 | libm |

all at 1 ULP except `tanh` at 2. So there is no row where switching backing
would help: the current split is already the closest available choice
everywhere. (`tanh` ties because glibc's `tanh` *is* the fdlibm body, so the two
candidates are the same function on Linux.)

Regenerate the same oracle with
`-XX:+UnlockDiagnosticVMOptions -XX:DisableIntrinsic=_dsin,_dcos,_dtan,_dexp,_dlog,_dlog10,_dpow,_dcbrt,_dtanh`
— which makes HotSpot run the Java `StrictMath` bodies instead of its Intel LIBM
stubs — and the right-hand column becomes **0 / 5400 on every row**. CratonVM's
fdlibm port reproduces HotSpot's non-intrinsic arithmetic exactly.

That pins the residue precisely: what remains between CratonVM and HotSpot on
`java.lang.Math` *is* HotSpot's Intel LIBM intrinsics, which agree with neither
fdlibm nor glibc, and closing it would mean carrying a bit-compatible
reimplementation of those stubs. It is at most 1 ULP on the rows these tests
touch, and the deterministic replica above shows it moved none of the 72
reproducible optimizer rows.

(HotSpot is at least self-consistent about it: a `-Xint` oracle is byte-identical
to the JIT one, so the intrinsic stubs back the interpreter too.)

## Part 2 — the defect this turned up: `Math.pow`

### What was wrong

`native_math_pow` carried an integer-exponent shortcut, introduced as a
"HotSpot-style fast path":

```rust
if a.is_finite() && b.is_finite() && b.fract() == 0.0 && b.abs() < 64.0 {
    let bi = b as i32;
    return Ok(Some(Value::Double(a.powi(bi))));
}
```

HotSpot has no such path — `_dpow` is the general Intel LIBM algorithm at every
exponent. `powi` is repeated squaring, which rounds once per multiplication, and
`java.lang.Math.pow` is specified as *"the computed result must be within 1 ulp
of the exact result"*. Measured against a HotSpot oracle over 1,040 bases per
exponent:

| exponent | disagrees | max ULP |
| --- | --- | --- |
| `-1`, `0`, `1`, `2` | 0 / 1040 | 0 |
| `3` | 273 / 1040 | 1 |
| `-2` | 299 / 1040 | 1 |
| `8` | 783 / 1040 | 4 |
| `17` | 900 / 1040 | 9 |
| `31` | 980 / 1040 | 20 |
| `62` | 1008 / 1040 | **40** |
| `≥ 64` (outside the path) | 0 / 1040 | 0 |

**24,026 of 35,360** sampled `|b| < 64` inputs wrong, against **1 of 6,240** for
the `powf` path immediately outside it.

A second, independent bug sat in the same function. Its comment claimed the
special values "fall through to powf, preserving Java/JLS special-value
semantics (… `pow(1, ±inf) == NaN` per JLS …)". Falling through to `powf` is
exactly what produces the **C99** answer, and C99 `pow(1, y)` is `1.0` for every
`y` including NaN and infinity. The JLS makes the exponent dominant there. All
five affected rows returned `1.0` where HotSpot returns NaN:

```
                     CratonVM (before)   HotSpot
pow( 1.0, NaN)       3ff0000000000000    7ff8000000000000
pow( 1.0, +inf)      3ff0000000000000    7ff8000000000000
pow( 1.0, -inf)      3ff0000000000000    7ff8000000000000
pow(-1.0, +inf)      3ff0000000000000    7ff8000000000000
pow(-1.0, -inf)      3ff0000000000000    7ff8000000000000
```

The comment stated the rule; the code did the opposite of it.

### Why no existing census caught either

The `Math` census draws **both** operands from a continuous random generator, so
an exponent that is exactly a whole number essentially never occurs and the fast
path is never entered — `pow` came back as "1 disagreement in 5400", the
second-best row in the table. Real numeric code does the opposite:
commons-math's `TestFunction.SUM_POW` is `Math.pow(abs(x), i + 2)` and `PERM` is
`Math.pow(j + 1, i + 1)`. A census whose input distribution excludes the
feature's own trigger reports the feature as healthy.

The special-value rows were missed for the ordinary reason: they were never
enumerated.

### The fix

`native-builtins/src/lang_math.rs`:

* the JLS rows are handled explicitly before anything else — `b.is_nan()`, or
  `b.is_infinite()` with `a.abs() == 1.0`, returns the canonical NaN;
* the integer shortcut is cut down to the four exponents where it rounds exactly
  once and is therefore the correctly-rounded power: `x^0 = 1`, `x^1 = x`,
  `x^2 = x*x`, `x^-1 = 1/x`. Each measured 0 / 1040. `x^-2` is deliberately not
  in the list — `1/(x*x)` rounds twice and missed on 299 of 1040;
* everything else goes to `powf`.

### After

| | `|e| < 64` | max ULP |
| --- | --- | --- |
| before | 24026 / 35360 | 40 |
| after, `--nojit` | **24 / 35360** | **1** |
| after, JIT | **24 / 35360** | **1** |

and all five special-value rows now return HotSpot's exact bit pattern on both
routes. `StrictMath.pow` was 0 / 41600 throughout and is untouched.

No other row regressed: the full 60-function `Math` census and the 98,400-row
`AccurateMath` census are unchanged on the fixed binary, and the 72 deterministic
`SoProbe` rows still match HotSpot bit-for-bit.

### Regression cover

Three tests in `native-builtins/src/lang_math.rs`, all verified to **fail** on
the pre-fix body before being accepted:

* `math_pow_at_integer_exponents_is_within_one_ulp` — three bit patterns
  captured from Temurin JDK 25, plus the rule itself asserted as a ULP bound
  against `fdlibm::pow` for every exponent in `-63..=63` over twelve bases. Not
  pinned to libm's own bits, because `Math.pow` *is* the host libm and its exact
  value is a platform fact.
* `math_pow_follows_the_jls_not_c99_when_the_base_is_one` — the five NaN rows
  and the four neighbours (`pow(NaN, ±0)`, `pow(inf, 0)`, `pow(1, 0)`) that must
  not be swept up with them.
* `math_and_strictmath_stay_split_on_intrinsified_rows` — the missing direction
  of the existing `math_and_strictmath_agree_on_non_intrinsified_rows` ratchet.
  That one fails when a shared row is split and is blind to a split row being
  merged; merging is the cheaper-looking edit and the one that costs accuracy,
  by the table in Part 1.

## Adjudication of the original filing, item by item

| filed claim | verdict |
| --- | --- |
| `SimplexOptimizer*` iteration counts diverge from HotSpot | **not reproducible.** 72 of 102 deterministic rows bit-identical; the other 30 are randomised inside `HedarFukushimaTransform` |
| `FastSineTransformerTest` shows a ULP-level divergence | **not a comparison.** Unseeded input, fails 18/18 on HotSpot |
| `MiniBatchKMeansClustererTest`, `CalinskiHarabaszTest` are downstream of it | **closed by the `Math.hypot` fix**, and their residual rate matches HotSpot's |
| `CorrelatedVectorFactoryTest` may be an RNG-algorithm difference | **no.** 7/18 on both arms; CratonVM's unseeded seeding measured healthy |
| `LevenbergMarquardtOptimizerTest.testControlParameters` may belong to this cluster | **no.** Zero failures in 48 CratonVM runs; closed by the `Math.hypot` fix |
| "check whether these use `Math` vs `StrictMath` inconsistently … `AccurateMath`" | **checked.** `AccurateMath` is 0/98,400 against HotSpot; `TestFunction` uses `Math` directly, which is what exposed the `pow` defect |
| residual `Math` divergence on the intrinsified rows | **characterised, not closed.** It is HotSpot's Intel LIBM stubs; libm is the closer of the two available backings on every row; fdlibm is 0/5400 against HotSpot-without-intrinsics |

## Method note worth keeping

Two failure modes of measurement showed up here, and both are cheap to avoid:

1. **A census is blind wherever its input distribution excludes the code path.**
   Random `(base, exponent)` pairs never produce a whole-number exponent, so a
   whole-number-exponent fast path was invisible to the instrument that was
   supposed to cover `pow`. When a function has a special case, sample it.
2. **Before comparing two VMs on a test, check whether the test seeds its RNG.**
   `grep -n 'RandomSource\.[A-Z_0-9]*\.create()'` over the test sources answers
   it in one command, and a `create()` with no argument means a single-run
   comparison carries no information — including a `create()` buried in the
   *library* the test calls, which is where a third of `SimplexOptimizerTest`
   turned out to hide one.

---

## Appendix — the filing as it stood, verbatim

Kept so this record is self-contained: everything above adjudicates the text
below, and nothing below has been edited.

# commons-math: cluster of iterative-numeric-algorithm and RNG-dependent tests diverging from HotSpot by small floating-point margins

## Status
**LARGELY CLOSED 2026-08-16 — root cause identified, and half the cluster
dissolves on re-measurement.** Everything below the "The evidence" heading is
the original filing, kept as written; this section records what was found
afterwards. Nothing here contradicts the original *observations*, only some of
the inferences drawn from them.

**The mechanism the doc could not isolate: `java.lang.Math` was backed by
platform libm where HotSpot runs fdlibm.** In JDK 25 every `Math`
transcendental is a one-line `return StrictMath.f(...)`, so the two classes are
the same function on HotSpot except where it substitutes an intrinsic — and
`asin`, `acos`, `atan`, `atan2`, `hypot`, `sinh`, `cosh`, `expm1` and `log1p`
have no intrinsic. Backing them with libm produced exactly the doc's shape:
ULP-scale differences, invisible in isolation, that change an iteration count.
A HotSpot-oracle census put numbers on it — `atan2` disagreed on 884 of 5400
sampled inputs, `hypot` on 405. Fixed by registering those rows from the
fdlibm port for `Math` as well as `StrictMath`. See the retired
`bug-commonsmath-gaussnewton-testmaxevaluations-no-exception-20260816`
write-up, which is where this was tracked down (a `hypot` ULP made an optimizer
converge in 9 evaluations instead of exceeding a 100-evaluation budget).

**Now passing** on the fixed binary, all previously failing:
`MiniBatchKMeansClustererTest` (the "genuinely different final clustering"),
`CalinskiHarabaszTest`, `SimplexOptimizerMultiDirectionalTest`.

**Not CratonVM defects — they fail on HotSpot too.** The doc's opening claim
that "all classes below pass on HotSpot" does not hold for three of them.
Measured directly, JDK 25, identical classpath:

| class | HotSpot | CratonVM (fixed) |
| --- | --- | --- |
| `SimplexOptimizerNelderMeadTest` | 1 failed | 3 failed |
| `SimplexOptimizerTest` | 36 failed | 43 failed |
| `FastSineTransformerTest` | **4 of 10 runs failed** | 6 of 10 |

`FastSineTransformerTest` deserves its own note, because it is the doc's
headline evidence — the "directly-measured ULP-level divergence". It builds its
input from `RandomSource.MWC_256.create()` with **no seed** and compares a
reference DST against the library's FFT to a 1e-13/1e-14 relative tolerance, so
each run draws fresh data and the assertion sits near the edge. It fails on
HotSpot 4 times in 10. A single observed failure of it is a coin flip, not a
measurement, and the specific value quoted in the doc cannot be reproduced
because the input that produced it no longer exists.

**Genuinely still open:** the residual gap on the two `SimplexOptimizer`
classes — 3 failures against HotSpot's 1, and 43 against 36. Much smaller than
filed, and now bounded by a HotSpot number rather than by "passes on HotSpot",
but not zero. The remaining `Math` divergences are `sin`, `cos`, `tan`, `exp`,
`log10`, `cbrt`, `tanh` and `pow`, all ≤2 ULP: HotSpot intrinsifies those, its
answers come from Intel LIBM assembly that matches neither fdlibm nor the host
libm, and libm is already the closer of the two by one to two orders of
magnitude. Whether that residue is what moves these two classes' iteration
counts is untested.

---

*Original filing follows.*

**OPEN, confirmed CratonVM-specific in aggregate, root cause NOT identified**
— found 2026-08-16 running commons-math under CratonVM on Azure.
Differential-verified against real HotSpot JDK 25: all classes below pass on
HotSpot with the identical classpath.

Filed as one cluster because six otherwise-unrelated test classes all show
the same underlying *shape* of divergence — a small (often ULP-scale or
low-single-digit-percent) numeric difference from HotSpot's result that is
large enough to flip an iteration-count or tolerance-based assertion, even
though the CratonVM result is not obviously wrong on its face. No specific
mechanism (a particular transcendental function, an RNG algorithm
difference, a summation-order difference) has been isolated yet — this is
deliberately filed as OPEN-not-root-caused rather than guessing, per this
project's own convention.

## The evidence

**Iteration-count divergence in derivative-free optimizers** — three
`SimplexOptimizer*` classes fail because CratonVM's optimizer converges
using a *different* number of function evaluations than HotSpot's, tripping
an assertion that checks `nEval` against an expected bound:
```
SimplexOptimizerNelderMeadTest:       AssertionError: ...nEval=111 < 110
SimplexOptimizerMultiDirectionalTest: AssertionError: ...FourExtrema@1d48: nEval=118
SimplexOptimizerTest (JUnit5):        AssertionFailedError: [ROSENBROCK dim=2]: nEval=15159 < 11000
```
An iterative optimizer's evaluation count is entirely determined by the
sequence of floating-point comparisons/arithmetic it performs along the
way — a divergent count (in either direction) means CratonVM is computing
at least one intermediate value slightly differently from HotSpot at some
point in the iteration, not that the final answer is wrong.

**A directly-measured ULP-level divergence** — `FastSineTransformerTest`
fails on a single transformed value differing from the reference in the
16th-17th significant digit:
```
FastSineTransformerTest: expected:<-0.0028534599156989915> but was:<-0.0028534599156990748>
  (4, 3 (1.00000e-14, 2.85346e-17))
```
This is the same *scale* of discrepancy (single-ULP-ish) that would be
sufficient to change an iteration count in the optimizer tests above if it
occurred inside a hot inner loop (e.g. inside a trig/sqrt call the optimizer
uses for its convergence check).

**Downstream consumers of the same effect** — `MiniBatchKMeansClustererTest`
(`Different score ratio 55.5%!, diff points ratio: 53.8%` — a genuinely
different final clustering, not just a tolerance miss) and
`CalinskiHarabaszTest` (`expected:<1.0> but was:<0.9356...>`, an evaluation
metric computed *from* clustering output) both depend on iterative,
randomized clustering — plausible that the same class of small
per-iteration floating-point divergence, compounded over many iterations of
a data-dependent (chaotic) algorithm like k-means, produces a materially
different final cluster assignment. `CalinskiHarabaszTest`'s failure is
likely *entirely downstream* of `MiniBatchKMeansClustererTest`'s (same
package family, evaluation index computed over cluster output) rather than
an independent bug — not confirmed.

`CorrelatedVectorFactoryTest` (`expected:<-3.0> but was:<-2.924...>`, ~2.5%
off) is RNG/distribution-sampling-shaped and may belong to this cluster
(chaotic sensitivity to tiny per-call differences) or may be an independent
RNG-algorithm difference — not distinguished here.

**Possibly related, not confirmed**: `LevenbergMarquardtOptimizerTest.testControlParameters`
(plain `java.lang.AssertionError`, package `fitting.leastsquares`, same
"iterative numeric optimizer" family as the `GaussNewtonOptimizerWith*Test`
cluster documented separately in
`bug-commonsmath-gaussnewton-testmaxevaluations-no-exception-20260816.md`)
— flagged there as worth checking against this cluster too; not done in
this pass.

## What this is NOT (based on current evidence)
Not a correctness bug in the sense of "wrong formula" — the outputs are
close to HotSpot's, not arbitrarily wrong, and the FastSineTransformer case
in particular shows a difference at the level of floating-point rounding,
not an algorithmic error. Not yet confirmed to be the same root cause as
the `expected NaN but was NaN` cluster
(`bug-commonsmath-vector-nan-comparison-cluster-20260816.md`) — that one
was shown to NOT be a `Double`-semantics issue at the JDK primitive level;
this cluster hasn't had the same isolation work done to rule that out here
too.

## Next steps
* Pick the smallest, most directly measurable case —
  `FastSineTransformerTest` — and narrow which specific function call
  inside the FFT/sine-transform pipeline first diverges from HotSpot's bit
  pattern (compare `Double.doubleToLongBits` at each stage, not just the
  final output), the same isolation technique already used successfully for
  the cbrt interpreter-throughput probe in this session's other findings.
* Once a single diverging primitive operation is identified there, check
  whether the same operation appears in the `SimplexOptimizer*` convergence
  check and in k-means's distance/centroid computation — that would confirm
  (or rule out) a single shared root cause across the whole cluster instead
  of several coincidentally-similar-looking bugs.
* Check whether any of these use `Math` vs `StrictMath` inconsistently
  between the JDK and commons-math's own `AccurateMath`
  (see the sibling finding
  `bug-commonsmath-accuratemathstrictcomparisontest-native-divide-overflow-panic-20260816.md`
  for a confirmed CratonVM-native-vs-StrictMath divergence in a *different*
  method family — worth checking if `AccurateMath`'s trig/sqrt functions
  have a similar CratonVM-native-vs-JDK-bytecode split that could explain a
  systematic ULP-level difference).

## Repro
```bash
cd apps/commons-math
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>" org.junit.platform.console.ConsoleLauncher \
  --disable-banner --disable-ansi-colors --select-class \
  org.apache.commons.math4.transform.FastSineTransformerTest
# or: SimplexOptimizerTest / SimplexOptimizerNelderMeadTest /
#     SimplexOptimizerMultiDirectionalTest / MiniBatchKMeansClustererTest /
#     ml.clustering.evaluation.CalinskiHarabaszTest / random.CorrelatedVectorFactoryTest
```
