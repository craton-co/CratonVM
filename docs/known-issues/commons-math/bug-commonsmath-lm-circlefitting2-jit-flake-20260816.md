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
