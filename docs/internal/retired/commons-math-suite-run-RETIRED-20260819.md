# Apache Commons Math 4.0-SNAPSHOT on CratonVM — RETIRED 2026-08-19, at parity with HotSpot

**Supersedes** the 2026-08-19 Windows rerun (`apps/commons-math/RESULTS-20260819.md`,
never committed — `apps/` is gitignored) and, through it,
retired/commons-math-suite-run-RETIRED-20260818.md.

**Retired because the suite reaches HotSpot's own pass count.** Same harness,
same 90 s per-class budget, same parallelism, same host, same afternoon:

| | PASS | FAIL | HANG |
|---|---:|---:|---:|
| **HotSpot 25** (Temurin, `-Xmx1g`) | 342 | 10 | 0 |
| **CratonVM** `39e7611f3` | **342** | 9 | 1 |

There is exactly **one** non-PASS with no HotSpot counterpart:
`stat.descriptive.rank.PSquarePercentileTest`, a throughput gap, tracked on
`known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`.

## Conditions

**Binaries:** `/data/bin/cratonvm-cmres-perf2` (md5 `c2139fcd3f…`), built from
`fix/commons-math-residuals-20260819`; HotSpot is `/data/toolchain/jdk-25`.
**Backend:** `--java-home` real JDK 25, real-jdk mode, JIT on, default GC (ZGC).
**Harness:** `CratonRunner`, one class per process, 90 s timeout, `--Xmx 1g`,
`xargs -P 2` — the *same* parallelism for both VMs, which matters because the
budget is wall clock.
**Host:** the shared Azure Linux box (8 core), other sessions building
throughout. Every number here that a reader might act on is either a COUNT or an
interleaved comparison for that reason.

**Population: 352 classes**, defined as every class under the six modules'
`target/test-classes` whose name ends in `Test`, plus `EvaluationTestValidation`
(which does not). The superseded doc used 309 by an unrecorded rule; 352 is a
superset and is reproducible from one `find`, which is why it was re-derived
rather than inherited.

## What changed since the superseded run

Four fixes, all on `fix/commons-math-residuals-20260819`, each behind a kill
switch so every claim below is a one-binary A/B.

### 1. OSR entry deferred to an oop mask that never ran — the big one

`BOBYQAOptimizerTest`: **HANG (≥400 s) → PASS (14.2 s, 17/17)** against
HotSpot's 11.8 s. Kernel probe `BobyqaOne 12 1`: **22.8-26.3 s → 1.0-1.4 s**,
four interleaved pairs, bit-identical result. `osr_refused_entry` **5 706 → 0**.

Full write-up:
fixed-bugs/osr-entry-deferred-to-an-oop-mask-that-never-ran-FIXED-20260819.md.
The one-line version: `BOBYQAOptimizer.trsbox` reuses local 87 as a `double`, an
`int` and a reference; the per-bci dataflow settled it as `Ref`; `kind_at`
discarded that answer because "the oop mask is the sole authority" — and the
mask read `oop_reached=false oop_mask=0x0` at every snapshot in the method.

### 2. ZGC publishes its arena envelope into `JIT_READ_BOUNDS`

`jit_getfield` helper calls on the accessor probe **17.8-18.0 M → 0**; on
`BobyqaOne` **2 125 738 → 0**. Worth ~1.08x of `BobyqaOne`'s wall clock and no
more — a count reaching zero is not a speedup of anything in particular. Closes
the ZGC row of
`known-issues/jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md`.

### 3. `MonitorTable::prune_dead` walked every dead address to remove nothing

5.36% → 1.65% of `LegendreHighPrecisionTest`.

### 4. The class-initialized memo thrashed past 64 classes

3.11% of `LegendreHighPrecisionTest` → absent from its top 12.

(3) and (4) together, three interleaved pairs, standalone:

| class | before | after |
|---|---:|---:|
| `LegendreHighPrecisionTest` | 87.5 / 123.9 / 95.8 s | **80.7 / 79.7 / 81.8 s** |
| `PSquarePercentileTest` | 164.0 / 171.2 / 131.2 s | **149.7 / 147.1 / 124.2 s** |

3 of 3 pairs each. Note the spread, not just the median: the Legendre after-arm
ranges 2.1 s where the before-arm ranged 36.4 s. That class was sitting where
host load decided its verdict; it no longer is.

## The complete non-PASS list, both VMs

Seven classes fail on **both**:

```
analysis.interpolation.UnivariatePeriodicInterpolatorTest
fitting.leastsquares.EvaluationTestValidation
ml.clustering.MiniBatchKMeansClustererTest
optim.nonlinear.scalar.noderiv.SimplexOptimizerTest
random.CorrelatedVectorFactoryTest
transform.FastCosineTransformerTest
transform.FastSineTransformerTest
```

Three fail on **HotSpot only** — CratonVM passed them in this sweep:

```
analysis.function.LogitTest
optim.nonlinear.scalar.noderiv.SimplexOptimizerMultiDirectionalTest
optim.nonlinear.scalar.noderiv.SimplexOptimizerNelderMeadTest
```

Three fail on **CratonVM only**, and two of the three were measured failing on
HotSpot in repeat runs the same afternoon:

| class | CratonVM | HotSpot, repeated |
|---|---|---|
| `transform.FastFourierTransformerTest` | FAIL | **fails 1 of 3** (`rel=6.1e-11` against the test's own tolerance) |
| `fitting.leastsquares.LevenbergMarquardtOptimizerTest` | FAIL | **fails 2 of 3** |
| `stat.descriptive.rank.PSquarePercentileTest` | **HANG** | passes, 9.9 s |

**`PSquarePercentileTest` is the only genuine CratonVM-only residual in the
suite.** It completes and passes standalone in 124-150 s; its profile is
allocation rate (`alloc_raw_tlab` 8.5%, `jit_newarray` 4.4%, GC 3%) and native
dispatch (~19%), with no member above 8.5% — the general "make native→heap
interaction cheap" problem, not anything commons-math-specific.

## Corrections to the superseded doc

Recorded because each was a claim someone would otherwise carry forward.

**"23 pre-existing, unseeded-RNG-driven test flakes, cross-checked against
HotSpot … comparable failure rates on both."** Not true of that list on current
`dev`. Running each of the 23 ten times on HotSpot: **18 of them failed 0 times
in 10 runs**, and the same 18 pass on CratonVM. The claim holds for five
(`EvaluationTestValidation`, `LevenbergMarquardtOptimizerTest`,
`UnivariatePeriodicInterpolatorTest`, `CMAESOptimizerTest`,
`SimplexOptimizerNelderMeadTest`) and is stale for the rest — a
cross-check inherited from an earlier run rather than re-taken.

**"`SimplexOptimizerTest` — new HANG since 08-17, not yet filed; likely the same
numeric-kernel cost as BOBYQA, unconfirmed."** It is not a CratonVM defect at
all. HotSpot fails it in **3 of 3 runs with 33 / 42 / 39 failing parameterised
cases** (43 in the sweep). It is an unseeded-RNG simplex test that never passes
on either VM; CratonVM's HANG-vs-FAIL label was only ever a question of whether
~40 slow cases finished inside 90 s. On this run it finished: FAIL, 37 of 102.

**"`AccurateMathTest` rc=127, no output … standalone retry did not finish within
2 min."** It now passes in 59.7 s in-sweep.

**"Two classes produced no output at all … read as shared-host timing
pressure."** Confirmed, and generalised: at `-P 4` the sweep showed **8** HANGs,
of which **every one completed and passed standalone**. Parallelism, not the
binary, decides that label — which is why this run holds parallelism equal
between the two VMs and reports the HotSpot control beside every number.

## Reproduction

```bash
source /data/toolchain/env.sh
source /data/cm-setup.sh            # builds CM_CP over the six modules
/data/cm-sweep.sh HOTSPOT                          /data/sweep-hs  /data/cm-pop.txt 2 90
/data/cm-sweep.sh /data/bin/cratonvm-cmres-perf2   /data/sweep-cv2 /data/cm-pop.txt 2 90
```

Per-class logs and `results.tsv` live under those two directories on the Azure
box; they are ephemeral and not committed.

## Slowest passing classes, both VMs

The shape of what is left, and the honest picture of the remaining gap:

| class | CratonVM | HotSpot |
|---|---:|---:|
| `analysis.integration.gauss.LegendreHighPrecisionTest` | 86.3 s | 6.4 s |
| `distribution.EmpiricalDistributionTest` | 66.2 s | 6.9 s |
| `core.jdkmath.AccurateMathTest` | 59.7 s | 15.1 s |
| `optim.nonlinear.scalar.noderiv.CMAESOptimizerTest` | 49.7 s | 6.6 s |
| `analysis.integration.IterativeLegendreGaussIntegratorTest` | 29.7 s | 3.1 s |
| `linear.BlockFieldMatrixTest` | 22.4 s | 5.9 s |
| `analysis.differentiation.SparseGradientTest` | 14.4 s | 6.1 s |

`BOBYQAOptimizerTest` is no longer in this list.
