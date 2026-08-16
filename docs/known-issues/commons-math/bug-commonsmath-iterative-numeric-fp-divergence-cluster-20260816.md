# commons-math: cluster of iterative-numeric-algorithm and RNG-dependent tests diverging from HotSpot by small floating-point margins

## Status
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
