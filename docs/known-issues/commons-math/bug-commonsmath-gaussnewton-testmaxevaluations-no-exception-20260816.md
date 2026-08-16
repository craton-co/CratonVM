# `GaussNewtonOptimizerWith*Test.testMaxEvaluations` — optimizer doesn't throw when max evaluations is exceeded

## Status
**OPEN, confirmed CratonVM-specific** — found 2026-08-16 running commons-math's
`commons-math-legacy` test suite under CratonVM on Azure
(`azureuser@20.80.105.49`). Differential-verified against real HotSpot JDK 25
(same classpath, same JUnit-Vintage-via-console-launcher harness): **HotSpot
passes all four**.

## The failure
Identical shape across all four `GaussNewtonOptimizerWith{Cholesky,LU,QR,SVD}Test`
classes:
```
Failures (1):
  JUnit Vintage:GaussNewtonOptimizerWithCholeskyTest:testMaxEvaluations
    => java.lang.AssertionError: Expected Exception from: GaussNewtonOptimizer{decomposition=CHOLESKY}
       org.junit.Assert.fail(Assert.java:89)
       org.apache.commons.math4.legacy.fitting.leastsquares.AbstractLeastSquaresOptimizerAbstractTest.fail(AbstractLeastSquaresOptimizerAbstractTest.java:85)
```
(Only the `decomposition=` value differs between the four.) `testMaxEvaluations`
configures the optimizer with a deliberately tiny evaluation budget and
expects it to throw (presumably `TooManyEvaluationsException`) once that
budget is exceeded. Under CratonVM, no exception is thrown — the shared test
helper (`AbstractLeastSquaresOptimizerAbstractTest.fail`, one common base
class behind all four decomposition variants) calls `fail()` because the
expected-exception block completed normally instead of throwing.

Since all four decomposition strategies (Cholesky/LU/QR/SVD — genuinely
different linear-algebra code paths) hit exactly the same test in exactly the
same way, the shared root cause is almost certainly in
`GaussNewtonOptimizer`'s own evaluation-count bookkeeping/limit check (used
by all four regardless of decomposition), not in any one decomposition's
implementation.

## Next steps
* Read `GaussNewtonOptimizer`'s evaluation-counting logic (likely a shared
  `LeastSquaresProblem$Evaluation`/evaluation-counter wrapper in
  `commons-math4-core` or `commons-math-legacy-core`) and instrument it to
  see whether the counter under CratonVM ever reaches the configured max —
  i.e. whether the optimizer converges/exits for an unrelated reason before
  the counter would trip (masking the intended max-evaluations path
  entirely), or whether the counter increments but the throw condition
  itself doesn't fire.
* Check whether this shares a cause with the LevenbergMarquardtOptimizerTest
  `testControlParameters` failure in the same package (also CratonVM-specific,
  also in `fitting.leastsquares`) — different specific assertion, but worth
  ruling in/out given the package-level clustering before assuming unrelated.

## Repro
```bash
cd apps/commons-math/commons-math-legacy
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>" org.junit.platform.console.ConsoleLauncher \
  --select-class org.apache.commons.math4.legacy.fitting.leastsquares.GaussNewtonOptimizerWithCholeskyTest
```
Reproduces on all four decomposition variants; confirmed absent on stock
HotSpot with the identical classpath.
