# `AutoScalingEventExecutorChooserFactoryTest.testScaleUp` — scaled to 3 executors instead of 2

## Status
New finding, 2026-08-29. Reproduced once on a quiet single-shard host (no
other suite concurrently running). Not yet cross-checked against HotSpot, not
yet re-run for reproducibility, not root-caused.

## Symptom

```
org.opentest4j.AssertionFailedError: Should scale up to 2 after stressing one executor. ==> expected: <2> but was: <3>
	at io.netty.util.concurrent.AutoScalingEventExecutorChooserFactoryTest.testScaleUp(AutoScalingEventExecutorChooserFactoryTest.java:152)
```

The test stresses one executor and asserts the chooser factory scales up to
exactly 2 live executors; CratonVM's run scaled to 3 instead — more
aggressively than expected, not less.

## Why this might be a timing artifact rather than a hard bug

The class name and assertion shape ("scale up after stressing") suggest a
threshold-triggered auto-scaling policy that's sensitive to how much work
gets done inside a stress window before the scale-up check fires. If
CratonVM's interpreter/JIT executes the stress workload measurably faster or
slower than whatever this test's threshold was tuned against, more scaling
events could plausibly fire before the assertion point — a real behavioral
difference, but one that could trace back to raw throughput rather than a
scaling-logic defect per se. Not confirmed either way.

## Not yet done
- HotSpot A/B on the identical class.
- A second CratonVM run to see if the scaled-to-3 result is deterministic or
  itself variable (which would support the timing-sensitivity hypothesis).
- Reading `AutoScalingEventExecutorChooserFactory`'s actual scale-up trigger
  logic to see what it keys on (call count, elapsed time, queue depth) before
  guessing further.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.concurrent.AutoScalingEventExecutorChooserFactoryTest
```
