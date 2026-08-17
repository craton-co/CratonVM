# ⛔ RETIRED — `LevenbergMarquardtOptimizerTest.testCircleFitting2` is not a CratonVM defect and is not JIT-specific

## Status

**RETIRED 2026-08-17** on branch `fix/commonsmath-fp-residuals-20260816`. The
filing's two load-bearing claims were both single-sample observations of a test
that is randomised by construction, and neither survives an interleaved
measurement:

* "CratonVM-specific: HotSpot JDK 25 passes the class" — **HotSpot fails it
  more often than CratonVM does.**
* "It is JIT-path-only. Under `--nojit` the class is 25/25 green" — **`--nojit`
  fails it at the same rate as the JIT arm.**

Nothing about the doc's *observations* is disputed; the failures it recorded
were real. What was wrong was reading them as a comparison.

## Why the test is a coin flip

`testCircleFitting2` builds its point cloud from

```java
final RandomCirclePointGenerator factory
    = new RandomCirclePointGenerator(xCenter, yCenter, radius, xSigma, ySigma);
```

and that constructor is

```java
final UniformRandomProvider rng = RandomSource.XO_SHI_RO_256_PP.create();
```

— **no seed**, so every run fits a different ten-point cloud. The test's own
source carries the comment `// The test is extremely sensitive to the seed.`
directly above the generator. It then asserts the fitted centre and radius lie
within two standard errors of the truth, which for ten noisy points is a margin
the fit misses a third of the time whatever executes it.

## The measurements

All runs on Azure host 2, `commons-math-legacy`, JDK 25, identical classpath,
arms **interleaved** run-for-run (`HS NOJIT JIT JIT NOJIT HS`) so that host load
— which ranged from 20 to 45 throughout — is shared rather than assigned to one
arm.

**The single method, 30 runs per arm** (`--select-method`):

| arm | failures |
| --- | --- |
| HotSpot JDK 25 | 9 / 30 |
| CratonVM `--nojit` | 10 / 30 |
| CratonVM JIT | 13 / 30 |

**The whole class, 24 runs per arm** (`--select-class`, the doc's own command):

| arm | runs with any failure | `testCircleFitting2` | `testParameterValidator` |
| --- | --- | --- | --- |
| HotSpot JDK 25 | 11 / 24 | 10 | 2 |
| CratonVM `--nojit` | 9 / 24 | 8 | 2 |
| CratonVM JIT | 8 / 24 | 7 | 2 |

Combined over both harnesses, `testCircleFitting2` failed 19 times on HotSpot,
18 on CratonVM `--nojit` and 20 on CratonVM JIT, out of 54 runs each.

`testParameterValidator` — not mentioned in the filing — fails 2 in 24 on
**every** arm including HotSpot, for the same reason: it builds its cloud from
the same unseeded generator. It is not a CratonVM defect either.

`testControlParameters`, which the filing named as the contrast case that the
`Math.hypot` fix had closed, did not fail once on any arm in 48 CratonVM runs.
That closure holds.

## The decisive experiment: the same test, seeded

Rates bound the claim statistically; a deterministic replica settles it. `LmProbe`
reproduces `testCircleFitting2` exactly — same generator construction order, same
`CircleProblem`, same `{118, 659, 115}` start, same `maxIterations(50)`,
`maxEvaluations(100)` and `SimpleVectorValueChecker(1e-6, 1e-6)` — with the one
change that the generator is seeded from a fixed constant. It prints raw bit
patterns for all ten generated points, the three fitted parameters, the three
asymptotic standard errors, the RMS, and the iteration and evaluation counts.

Over 40 seeds, **HotSpot, CratonVM `--nojit` and CratonVM JIT produce
byte-identical output**: 81 lines of hex, no diff, on either dispatch route.
16 of those 40 seeds fail the test's assertion — on all three arms, at the same
seeds.

So there is no arithmetic divergence on this path at all, in either the
interpreter or the JIT. What the filing recorded was 40%-ish of a coin flip
landing differently on different days.

## Why "`--nojit` 25/25 green" looked so convincing

The class has 25 test methods. A run that reports 25 passing tests is one run,
not 25 — and one clean run of a test that passes two times in three is not
evidence. Read as 25 independent runs it would have been overwhelming, which is
what made it persuasive; read correctly it carries about as much information as
a single coin coming up heads.

The general lesson, which is the reason this write-up is kept: **before
comparing two VMs on a test, check whether the test seeds its own RNG.** Every
class in the sibling
`bug-commonsmath-iterative-numeric-fp-divergence-cluster-20260816` filing turned
out to be unseeded too. `grep -n 'RandomSource\.[A-Z_0-9]*\.create()' ` over the
test source answers it in one command, and a `create()` with no argument means
any single-run comparison is uninterpretable.

## Reproducing

The rate harness and the seeded probe both live in this session's scratch on
Azure host 2 (`/data/mfp-lm3.sh`, `/data/mfp-lmcls.sh`, `/data/mfp/LmProbe.java`).
The probe is the useful one to keep: it is deterministic, so it can be rerun on
any future binary and diffed against HotSpot with no statistics at all.

```bash
javac -cp "$CP" -d classes LmProbe.java
java              -cp "classes:$CP" org.apache.commons.math4.legacy.fitting.leastsquares.LmProbe 40 > hs.txt
cratonvm --nojit  -c "classes:$CP" org.apache.commons.math4.legacy.fitting.leastsquares.LmProbe 40 > nojit.txt
cratonvm          -c "classes:$CP" org.apache.commons.math4.legacy.fitting.leastsquares.LmProbe 40 > jit.txt
diff hs.txt nojit.txt && diff hs.txt jit.txt
```

## What this investigation did turn up

Chasing the "the compiled code is not calling the natives it should" hypothesis
from the filing's next-steps list led to a full differential census of
`java.lang.Math` and of commons-math's own `AccurateMath`, and that census found
a genuine, spec-violating defect in `Math.pow` — unrelated to this test, but
found because of it. See
bug-commonsmath-iterative-numeric-fp-divergence-cluster-20260816-CLOSED.

---

## Appendix — the filing as it stood, verbatim

Kept so this record is self-contained: everything above adjudicates the text
below, and nothing below has been edited.

# `LevenbergMarquardtOptimizerTest.testCircleFitting2` fails ~2 runs in 3 — but only with the JIT on

## Status
**OPEN, not root-caused (2026-08-16).** CratonVM-specific: HotSpot JDK 25 passes
the class on the identical classpath. Filed as a spin-off of the
`GaussNewtonOptimizerWith*Test.testMaxEvaluations` investigation, which ruled it
out as a regression from that fix.

## What is established

Azure host 2, `commons-math-legacy`, JUnit-Vintage via console-launcher.

**It is JIT-path-only.** Under `--nojit` the class is 25/25 green on the fixed
binary, in every run.

**It is not a regression from the `Math.hypot` fix.** Six runs, ABBA-interleaved
between the fixed binary (`FIX`) and the pristine one the bug was found on
(`PRI`), JIT enabled, listing the failing test names:

```
FIX -> [testCircleFitting2]
PRI -> [testCircleFitting2 testControlParameters]
PRI -> [testControlParameters]
FIX -> [testCircleFitting2]
FIX -> [none]
PRI -> [testCircleFitting2 testControlParameters]
```

`testCircleFitting2` appears in 2 of 3 runs on **each** binary, so it predates
the change. (`testControlParameters` is the contrast: 3 of 3 on the pristine
binary, 0 of 3 on the fixed one — that one was the `Math.hypot` bug and is
closed.)

**It is non-deterministic.** One of the three `FIX` runs was clean. The host
carried a load average of 9-26 from sibling agents throughout, so a
load-sensitive JIT decision (compile threshold reached or not, OSR entry taken
or not) is the obvious shape to check first — see
`docs/known-issues/unit-test-load-sensitive-flakes-20260815.md` for the same
shape in the Rust unit suite.

## Next steps

* Get the assertion text, not just the test name: the runs above were reduced to
  names for the ABBA comparison. `testCircleFitting2` fits a circle from a
  larger point set and asserts on the centre and radius, so the message will say
  whether this is a wrong number or an exception.
* Re-run under `--nojit` and with JIT tier thresholds pinned, to separate "the
  JIT compiles a method it should not" from "the compiled code is wrong".
* If the failure is numeric, diff the compiled path's arithmetic against the
  interpreter's for the same inputs — the JIT has no intrinsic for any of the
  transcendentals this test reaches, so a divergence there would mean the
  compiled code is not calling the natives it should.

## Repro

```bash
cd apps/commons-math/commons-math-legacy
<cratonvm-bin> --java-home <jdk25-home> --Xmx 1g \
  -c "<full commons-math classpath>" org.junit.platform.console.ConsoleLauncher \
  --select-class org.apache.commons.math4.legacy.fitting.leastsquares.LevenbergMarquardtOptimizerTest
```

Note the absence of `--nojit`: with it, the class passes.
