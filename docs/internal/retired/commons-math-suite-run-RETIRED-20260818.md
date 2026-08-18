# ✅ RETIRED — Apache Commons Math suite run (2026-08-17), all eleven CratonVM-only classes adjudicated

## Status

**RETIRED 2026-08-18.** Supersedes the working-tree page
`apps/commons-math/RESULTS-20260817.md` (never tracked — it lived in the
untracked `apps/commons-math/` checkout and is deleted with this record).

The 2026-08-17 run itemised **11 classes that failed only on CratonVM**. Every
one is now closed:

| outcome | count | classes |
|---|---:|---|
| fixed, verified re-running the class | 5 | `DerivativeStructureTest`, `FunctionUtilsTest`, `FiniteDifferencesDifferentiatorTest`, `NordsieckStepInterpolatorTest`, `SparseRealVectorTest` |
| fixed enough to clear the suite budget | 1 | `LegendreHighPrecisionTest` (HANG → PASS) |
| re-filed as a general VM throughput gap, not a commons-math defect | 1 | `BOBYQAOptimizerTest` |
| confirmed pre-existing test flakiness (fails on HotSpot too) | 4 | `LogitTest`, `UnivariatePeriodicInterpolatorTest`, `MultiStartMultivariateOptimizerTest`, `CorrelatedVectorFactoryTest` |

Nothing on this page is open. The two throughput pages it hands off to are
`docs/known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`
and
`docs/known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`.

## The closing run

Azure Linux (8 core), `dev` at `c6299ca2a` plus
`perf/bigdecimal-native-overhead-20260818`. Real JDK 25 backend (Temurin
25.0.4), real-jdk mode, JIT on, default GC (ZGC), `--Xmx 1g`, one JUnit 5 class
per process through `CratonRunner`, 90 s per-class cap — the same harness and
budget the original run used, so the two are comparable.

Population: **all 310 test classes of `commons-math-legacy`**. That is the
module every one of the 11 lives in; the original run's 309 spanned six modules,
so the totals below are not the same denominator as the original headline table
and are not compared to it.

| status | count |
|---|---:|
| PASS | **303** |
| HANG (>90 s) | 2 |
| FAIL | 5 |

**Every non-PASS is accounted for, and none of the seven is a new CratonVM
correctness defect:**

| class | CratonVM | HotSpot, same session | verdict |
|---|---|---|---|
| `optim…noderiv.BOBYQAOptimizerTest` | HANG | PASS 2 s | throughput — `bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817` |
| `stat.descriptive.rank.PSquarePercentileTest` | HANG | PASS | throughput cliff, already investigated and reduced 2.6x — fixed-suite-bugs/bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md. Terminates correctly given a large enough budget; 90 s is below the cost of the work |
| `optim…noderiv.CMAESOptimizerTest` | FAIL | FAIL | unseeded `RandomSource.MT_64.create()`. Repeated 3x per arm: fails 1/3 on CratonVM, **2/3 on HotSpot** |
| `optim…noderiv.SimplexOptimizerTest` | FAIL | FAIL | fails identically on HotSpot |
| `ml.clustering.MiniBatchKMeansClustererTest` | FAIL | FAIL | fails identically on HotSpot |
| `fitting.leastsquares.LevenbergMarquardtOptimizerTest` | FAIL | FAIL | fails identically on HotSpot |
| `analysis.interpolation.UnivariatePeriodicInterpolatorTest` | FAIL | FAIL | unseeded `RandomSource.KISS.create()`; flips on both VMs across runs |

## The eleven, one row each

Measured by re-running each class on three arms in the same session — the
current binary, a `dev`-base control built from the same tree, and HotSpot.

| class | 2026-08-17 | now | what closed it |
|---|---|---|---|
| `linear.SparseRealVectorTest` | CRASH | **PASS** 2 s | fixed-bugs/inline-trap-inside-a-protected-range-FIXED-20260818.md |
| `analysis.differentiation.DerivativeStructureTest` | FAIL | **PASS** 11 s | fixed-bugs/jit-multianewarray-allocated-every-level-with-classid-0-FIXED-20260817.md |
| `analysis.FunctionUtilsTest` | FAIL | **PASS** 2 s | same |
| `analysis.differentiation.FiniteDifferencesDifferentiatorTest` | FAIL | **PASS** 1 s | same |
| `ode.sampling.NordsieckStepInterpolatorTest` | FAIL | **PASS** 1 s | same — a second reader (`ObjectInputStream`'s reflective field restore) of the same wrong array class |
| `analysis.integration.gauss.LegendreHighPrecisionTest` | HANG | **PASS** 86-89 s | `perf/bigdecimal-native-overhead-20260818` — three per-call taxes removed from the bignum natives, 110-119 s → 86-89 s |
| `optim…noderiv.BOBYQAOptimizerTest` | HANG | HANG | re-filed: the OSR refusal it was blamed on was real and IS fixed, and the wall time did not move. It is compiled-code throughput |
| `analysis.function.LogitTest` | FAIL | **PASS** | pre-existing ~1/3 floating-point tolerance flake, reproduces on HotSpot |
| `analysis.interpolation.UnivariatePeriodicInterpolatorTest` | FAIL | FAIL both VMs | unseeded RNG |
| `optim…scalar.MultiStartMultivariateOptimizerTest` | FAIL | FAIL both VMs | unseeded RNG |
| `random.CorrelatedVectorFactoryTest` | FAIL | PASS here, FAIL on HotSpot here | unseeded RNG, flips both ways |

## Two things this page got wrong, kept because the shape recurs

1. **A GC diagnosis that named the wrong subsystem.** `DerivativeStructureTest`
   was written up as a precise-root-map gap: the reclaim guard printed
   `in_published_snapshot=false`, which reads as "GC collected a live frame's
   array". It fires on any object whose header decodes as the wrong class, not
   only on a genuinely reclaimed one. The real defect was in the x64 JIT's
   `multianewarray` lowering, allocating every level with `ClassId(0)`. Two
   controls settle that question in one command each and neither had been run:
   an 8 g heap, and `--XX:UseGc G1`. Both reproduce it identically, which rules
   the collector out.
2. **A per-call-overhead hypothesis reasoned from the shape of a number.** The
   `BigDecimal` page multiplied ~1.1M native calls by a 12-15 µs tax it inferred
   from the total, and concluded the fix was "very likely in the same family" as
   two already-fixed dispatch bugs. It flagged itself *not confirmed*, which was
   right: profiled, native dispatch is ~6%, and the three things that actually
   cost were an element-by-element `mag:[I` walk, a by-name field/class resolve
   on every call, and one method (`BigDecimal.signum()`) rendering the whole
   magnitude to a decimal string to read its first byte.

## Reproduction

Harness and class list live on the Azure Linux box: `/data/cm-sweep.sh`
(binary | `hotspot`, class list, output dir, per-class timeout),
`/data/cm-legacy-test-list.txt`, `/data/cm-legacy-classpath.txt`, and the
compiled `CratonRunner` in `/data/cm-runner`. `apps/netty-suite-runner/CratonRunner.java`
is the runner source.

```bash
/data/cm-sweep.sh <cratonvm-binary> /data/cm-legacy-test-list.txt /data/cm-out 90
/data/cm-sweep.sh hotspot           /data/cm-legacy-test-list.txt /data/cm-out-hs 90
awk -F'\t' '$3!="PASS"' /data/cm-out/results.tsv
```

Maven is not needed to re-run: `commons-math-legacy`'s `target/classes` and
`target/test-classes` are already built under
`/data/cratonvm/apps/commons-math/`, and every dependency jar is installed in
`/data/toolchain/m2repo`.
