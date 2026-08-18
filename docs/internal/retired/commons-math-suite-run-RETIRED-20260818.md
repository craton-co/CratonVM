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
| reduced 24% — now ON the suite budget rather than over it | 1 | `LegendreHighPrecisionTest` (HANG → borderline PASS, see the note below the table) |
| re-filed as a general VM throughput gap, not a commons-math defect | 1 | `BOBYQAOptimizerTest` |
| confirmed pre-existing test flakiness (fails on HotSpot too) | 4 | `LogitTest`, `UnivariatePeriodicInterpolatorTest`, `MultiStartMultivariateOptimizerTest`, `CorrelatedVectorFactoryTest` |

Nothing on this page is open. The closing sweep covers all six modules the
original run did — 351 classes, HotSpot run alongside as the control. The two
throughput pages it hands off to are
`docs/known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`
and
`docs/known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`.

## The closing run

Azure Linux (8 core), `dev` at `c6299ca2a` plus
`perf/bigdecimal-native-overhead-20260818`. Real JDK 25 backend (Temurin
25.0.4), real-jdk mode, JIT on, default GC (ZGC), `--Xmx 1g`, one JUnit 5 class
per process through `CratonRunner`, 90 s per-class cap — the same harness and
budget the original run used, so the two are comparable.

Population: **all six modules the original run covered** — 310 classes of
`commons-math-legacy` (the module every one of the 11 lives in) plus the 41
classes of `core`, `legacy-core`, `legacy-exception`, `neuralnet` and
`transform`. 351 classes, HotSpot run alongside as the control.

| | CratonVM | HotSpot |
|---|---:|---:|
| PASS | **341** | 344 |
| HANG (>90 s) | **3** | 0 |
| FAIL | 7 | 7 |

Both VMs fail seven classes, and they are **not the same seven** — the two lists
share three, and every class in the symmetric difference is an unseeded-RNG
tolerance check that flips from run to run on whichever VM happens to draw a bad
sample. Failing-class COUNT is therefore not a VM-quality signal on this suite;
only a per-class, repeated, both-VM comparison is, which is what the table below
is.

| | classes |
|---|---|
| fail on BOTH | `UnivariatePeriodicInterpolatorTest`, `LevenbergMarquardtOptimizerTest`, `SimplexOptimizerTest` |
| CratonVM only, this run | `MiniBatchKMeansClustererTest`, `CMAESOptimizerTest`, `FastCosineTransformerTest`, `FastSineTransformerTest` |
| HotSpot only, this run | `MultiStartMultivariateOptimizerTest`, `SimplexOptimizerNelderMeadTest`, `FastCosineTransformerTest` (2 of 3 repeats) |

**Every non-PASS is accounted for, and none of CratonVM's ten is a new
correctness defect:**

| class | CratonVM | HotSpot, same session | verdict |
|---|---|---|---|
| `optim…noderiv.BOBYQAOptimizerTest` | HANG | PASS 2 s | throughput — `bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817` |
| `stat.descriptive.rank.PSquarePercentileTest` | HANG | PASS | throughput cliff, already investigated and reduced 2.6x — fixed-suite-bugs/bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md. Terminates correctly given a large enough budget; 90 s is below the cost of the work |
| `optim…noderiv.CMAESOptimizerTest` | FAIL | FAIL | unseeded `RandomSource.MT_64.create()`. Repeated 3x per arm: fails 1/3 on CratonVM, **2/3 on HotSpot** |
| `optim…noderiv.SimplexOptimizerTest` | FAIL | FAIL | fails identically on HotSpot |
| `ml.clustering.MiniBatchKMeansClustererTest` | FAIL | FAIL when run alone | fails on HotSpot too when the class is run on its own |
| `fitting.leastsquares.LevenbergMarquardtOptimizerTest` | FAIL | FAIL | fails identically on HotSpot |
| `analysis.interpolation.UnivariatePeriodicInterpolatorTest` | FAIL | FAIL | unseeded `RandomSource.KISS.create()`; flips on both VMs across runs |
| `legacy.core.jdkmath.AccurateMathTest` | HANG | PASS | the other half of the PSquare throughput cliff, same fixed-suite-bugs record, same verdict: correct given a large enough budget, and 90 s is below the cost of the work |
| `transform.FastCosineTransformerTest` | FAIL | FAIL | unseeded `RandomSource.MWC_256.create()` (`RealTransformerAbstractTest:39`). Repeated 3x per arm: 0-1 failures on the fixed binary, 0-2 on `dev` base, **1-2 on HotSpot** |
| `transform.FastSineTransformerTest` | FAIL on `dev` base and once on HotSpot | — | same unseeded RNG; the deltas are ~1e-16 absolute against a 1e-13/1e-14 relative tolerance |

The transform pair is worth one line of method, because the first read of a
single sweep was "CratonVM fails a transform test HotSpot passes". Grepping for
the RNG before filing anything settled it: `RandomSource.MWC_256.create()` takes
no seed, so the two VMs are not running the same numbers, and repeating each arm
three times has HotSpot failing at least as often as CratonVM.

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
| `analysis.integration.gauss.LegendreHighPrecisionTest` | HANG | **PASS** 86 s in the sweep | `perf/bigdecimal-native-overhead-20260818` — three per-call taxes removed from the bignum natives, 110-120 s → 86-105 s. **Borderline:** see below |
| `optim…noderiv.BOBYQAOptimizerTest` | HANG | HANG | re-filed: the OSR refusal it was blamed on was real and IS fixed, and the wall time did not move. It is compiled-code throughput |
| `analysis.function.LogitTest` | FAIL | **PASS** | pre-existing ~1/3 floating-point tolerance flake, reproduces on HotSpot |
| `analysis.interpolation.UnivariatePeriodicInterpolatorTest` | FAIL | FAIL both VMs | unseeded RNG |
| `optim…scalar.MultiStartMultivariateOptimizerTest` | FAIL | FAIL both VMs | unseeded RNG |
| `random.CorrelatedVectorFactoryTest` | FAIL | PASS here, FAIL on HotSpot here | unseeded RNG, flips both ways |

### The one borderline row

`LegendreHighPrecisionTest` costs ~90 s, and the budget is 90 s, so which side of
it a run lands on is decided by host load as much as by the binary. Measured
over six INTERLEAVED rounds against a `dev`-base control built from the same
tree: the fixed binary won every round by 20-27% (86, 87, 89, 89, 96, 105 s
against 110, 112, 113, 118, 119, 120 s), and the control never once came in
under 90 s. It scored PASS in the 310-class sweep and in 4 of 7 timed runs.

The correct reading is "the fix is worth ~24% and moved this class onto the
budget", not "this class passes now". The rest of the gap — CratonVM 86-105 s
against HotSpot 2.7 s — is the general throughput residual the two known-issues
pages carry.

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
# the legacy module, from a class list
/data/cm-sweep.sh <cratonvm-binary> /data/cm-legacy-test-list.txt /data/cm-out 90
/data/cm-sweep.sh hotspot           /data/cm-legacy-test-list.txt /data/cm-out-hs 90

# the other five modules, discovering classes from target/test-classes
for m in core legacy-core legacy-exception neuralnet transform; do
  /data/cm-sweep-mod.sh $m <cratonvm-binary> /data/cm-out-mods/$m 90
  /data/cm-sweep-mod.sh $m hotspot           /data/cm-out-mods-hs/$m 90
done

awk -F'\t' '$3!="PASS"' /data/cm-out/results.tsv /data/cm-out-mods/*/results.tsv
```

`cm-sweep-mod.sh` puts `junit-platform-launcher` on the classpath explicitly —
the per-module `cm-commons-math-<mod>-cp.txt` files do not carry it, and without
it every class in those modules dies with `NoClassDefFoundError:
SummaryGeneratingListener` and scores CRASH. That is a harness gap that looks
exactly like a VM defect; check for the launcher before believing a whole module
crashed.

Maven is not needed to re-run: `commons-math-legacy`'s `target/classes` and
`target/test-classes` are already built under
`/data/cratonvm/apps/commons-math/`, and every dependency jar is installed in
`/data/toolchain/m2repo`.
