# commons-math: `AccurateMathTest` / `PSquarePercentileTest` exceed even a 10x timeout — confirmed CPU-bound interpreter throughput cliff, not a hang

## Status
**OPEN, confirmed CratonVM-specific** — found 2026-08-16 running commons-math
under CratonVM on Azure (`azureuser@20.80.105.49`). Both classes were
originally flagged as HANG at a 180s per-class timeout, then reran at a **10x
timeout (1800s / 30 min)** and still did not finish (`rc=124`). Real HotSpot
JDK 25 runs both classes to completion in single-digit seconds:
`AccurateMathTest` in 6s, `PSquarePercentileTest` in 7s.

Despite the HANG classification from the timeout-based harness, this is
**not confirmed to be a stuck/deadlocked/infinite-loop defect** — the
evidence below points at a severe but ordinary CPU-bound interpreter
throughput cliff (CratonVM runs in `--nojit`, pure-interpreter mode in this
harness) on two specific call-dense numeric shapes, compounded across many
structurally-identical large-workload test methods within each class.

## `AccurateMathTest`: confirmed to terminate correctly, just ~330x slower per iteration

`testCbrtAccuracy` (one of 17 structurally-identical `test*Accuracy` methods
in this class) runs a 10,000-iteration loop comparing `AccurateMath.cbrt(x)`
against an arbitrary-precision `Dfp` reference computed via
`DfpMath.pow(x, 1/3)` for each of 10,000 random inputs.

A verbose JUnit run (`--details=verbose`), killed at a 90s timeout, showed
the tree-printer's live progress had reached `testCbrtAccuracy` (with
`testFloorModInt`, `testSubtractExactInt`, `testAtan2SpecialCases` already
printed complete with SUCCESSFUL status) — i.e. it was still working through
the first few of the 17 accuracy tests after 90s.

Extracting the exact loop body of `testCbrtAccuracy` into a standalone,
scale-adjustable probe (`CbrtScaleProbe.java`, included below) and running it
at reduced N confirmed the loop **terminates with the correct result**, just
very slowly:

| N | HotSpot | CratonVM (`--nojit`) | per-iter ratio |
|---|---|---|---|
| 10,000 | 758ms total (0.076ms/iter) | *(not run at full N — see extrapolation)* | |
| 50 | *(negligible)* | 1248ms total (25ms/iter), `maxerrulp=0.0` (matches HotSpot), exits normally | **~330x** |

Extrapolating the measured per-iteration cost to the full 10,000-iteration
loop gives roughly 250s for `testCbrtAccuracy` alone. With 17 structurally
similar `test*Accuracy` methods in the same class, a conservative estimate
of ~4,200s (~70 minutes) total for the whole class is consistent with why
even the 1800s (30 min) 10x-timeout budget was exceeded, without requiring
any single method to be genuinely non-terminating.

## `PSquarePercentileTest`: the cheap part (`increment()`) is fast; the suite's own workload sizes are enormous

Isolating just the hot-path call (`PSquarePercentile.increment(double)`) into
a standalone probe (`PSquareScaleProbe.java`, included below) showed it is
**not** the bottleneck: 2,000 increments completed in 122ms under CratonVM
(~0.06ms/iter) — in line with ordinary interpreter overhead, and fast enough
that even the suite's largest dataset (990,000 elements, see below) would
only take roughly a minute via `increment()` alone.

The real cost is elsewhere in the class. Several test methods build very
large arrays via `randomTestData(factor, values)` and then call
`computePercentile()`, which delegates to
`Quantile.withDefaults().with(EstimationMethod.HF6).withCopy(copy).evaluate(test, percentile/100)`
— a comparator/sort-based reference computation over the **whole array**:

* `test20Percentile` — 100,000 elements
* `test5Percentile` — **990,000 elements**
* `test99PercentileHighValues` / `test90PercentileHighValues` — 10,000 /
  100,000 elements

A 990,000-element comparator-based sort is a call-dense operation (one
comparator invocation per comparison in the sort). This project's own prior
finding that lambda/comparator dispatch is a disproportionately expensive
shape under CratonVM's interpreter (independent of this investigation) is a
plausible explanation for why `Quantile...evaluate()` on a 990,000-element
array could cost far more than the equivalent linear `increment()` loop, but
this specific call was **not independently isolated and timed** in this
pass — flagged below as the concrete next step.

## Next steps
* Extract `Quantile.withDefaults().with(EstimationMethod.HF6)...evaluate()`
  into its own scaled probe (same pattern as `PSquareScaleProbe.java`) and
  time it directly at increasing N, to confirm/quantify the comparator-sort
  hypothesis for `PSquarePercentileTest` specifically.
* Re-run both classes with CratonVM's JIT enabled (drop `--nojit`) — this
  entire differential harness runs in forced-interpreter mode; if hot loops
  like these actually get JIT-compiled in normal CratonVM usage, this may be
  substantially a `--nojit`-mode artifact of the harness rather than a
  production-relevant defect. Worth confirming before treating this as a
  general throughput regression.
* If the gap persists even with JIT enabled, treat as a genuine perf-cliff
  bug report (not correctness) — profile which specific bytecode
  shapes/dispatch paths dominate the cost in `DfpMath.pow`'s call chain and
  in the `Quantile` comparator sort.
* Practically: exclude these two classes (or give them a much larger,
  multi-hour timeout) from timeout-bounded suite sweeps rather than
  continuing to classify them as HANG — the evidence here does not support
  a stuck/deadlocked interpretation.

## Repro
```bash
cd apps/commons-math
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>:<probeclasses dir>" CbrtScaleProbe 50
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>:<probeclasses dir>" PSquareScaleProbe 2000
```

`CbrtScaleProbe.java` (loop body lifted verbatim from
`AccurateMathTest.testCbrtAccuracy`, N made configurable):
```java
import org.apache.commons.math4.legacy.core.dfp.Dfp;
import org.apache.commons.math4.legacy.core.dfp.DfpField;
import org.apache.commons.math4.legacy.core.dfp.DfpMath;
import org.apache.commons.math4.core.jdkmath.AccurateMath;
import java.util.Random;

public class CbrtScaleProbe {
    static DfpField field = new DfpField(40);

    static Dfp cbrt(Dfp x) {
        boolean negative = false;
        if (x.lessThan(field.getZero())) {
            negative = true;
            x = x.negate();
        }
        Dfp y = DfpMath.pow(x, field.getOne().divide(3));
        return negative ? y.negate() : y;
    }

    public static void main(String[] args) throws Exception {
        int n = Integer.parseInt(args[0]);
        Random generator = new Random(42);
        long start = System.nanoTime();
        double maxerrulp = 0.0;
        for (int i = 0; i < n; i++) {
            double x = ((generator.nextDouble() * 200.0) - 100.0) * generator.nextDouble();
            double tst = AccurateMath.cbrt(x);
            double ref = cbrt(field.newDfp(x)).toDouble();
            double err = (tst - ref) / ref;
            if (err != 0) {
                double ulp = Math.abs(ref - Double.longBitsToDouble((Double.doubleToLongBits(ref) ^ 1)));
                double errulp = field.newDfp(tst).subtract(cbrt(field.newDfp(x))).divide(field.newDfp(ulp)).toDouble();
                maxerrulp = Math.max(maxerrulp, Math.abs(errulp));
            }
        }
        long end = System.nanoTime();
        System.out.println("DONE n=" + n + " elapsed_ms=" + (end - start) / 1_000_000 + " maxerrulp=" + maxerrulp);
    }
}
```

`PSquareScaleProbe.java`:
```java
import org.apache.commons.math4.legacy.stat.descriptive.rank.PSquarePercentile;
import java.util.Random;

public class PSquareScaleProbe {
    public static void main(String[] args) throws Exception {
        int n = Integer.parseInt(args[0]);
        Random generator = new Random(42);
        PSquarePercentile psquared = new PSquarePercentile(0.99);
        long start = System.nanoTime();
        for (int i = 0; i < n; i++) {
            double value = Math.abs(generator.nextDouble() * 100);
            psquared.increment(value);
        }
        long end = System.nanoTime();
        System.out.println("DONE n=" + n + " elapsed_ms=" + (end - start) / 1_000_000 + " result=" + psquared.getResult());
    }
}
```

## Related
Same overarching theme as the bc-java finding
`bug-bcjava-pqc-lms-hsstests-interpreter-throughput-cliff-20260816.md` — a
CPU-bound (not deadlocked) interpreter throughput cliff on call-dense
numeric/cryptographic workloads, found the same day in a parallel
investigation. Worth eventually asking whether both point at the same
underlying dispatch-cost mechanism, once either is profiled further.
