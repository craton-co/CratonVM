# `BeanRegistrationsAotContributionTests` is correct but ~100x slower than HotSpot, so every full-suite run reads it as TIMEOUT

| | |
|---|---|
| **Status** | OPEN. Correctness matches HotSpot; this is a throughput gap only. |
| **Scope** | `org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests` (spring-framework, `spring-beans`), dominated by `applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` (10001 bean definitions). |
| **Measured** | 2026-09-07, Azure host `20.80.105.49`, real JDK 25, binary built from `fix/spring-jit-aot-clusters-20260907`. |

## The numbers

| | status | found | succ | fail | wall |
|---|---|---:|---:|---:|---|
| HotSpot JDK 25 | OK | 14 | 14 | 0 | **10.7 s** |
| CratonVM, `--one-to 2400` | OK | 14 | 14 | 0 | **1067 s** |
| CratonVM, suite defaults (`--one-to 180`) | TIMEOUT | 0 | 0 | 0 | killed at the cap |

So the class is **not** failing. It reads as `TIMEOUT` in every full-suite run
purely because 1067 s does not fit the runner's 180 s per-class cap, and a
`TIMEOUT` row carries `found=0`, which is indistinguishable from a class that
could not start.

## Why this is being filed now

Until 2026-09-07 the class failed early, in `TestCompiler`, as part of the
19-class AOT cluster (see the retired
`testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster`
write-up). With that defect fixed the class runs its tests for the first time
in a while, and the throughput gap behind it became visible. This is the
"a deterministic failure hides the next one" shape: the fix did not cause the
slowness, it exposed it.

The *correctness* half of this test has its own closed record — the retired
`beanregistrations-verylarge-heap-footprint` write-up, fixed 2026-08-06, which
established that the last remaining failure "was never about the heap" and left
the class matching HotSpot. That page did not record a wall-clock number for
the class, so there is no baseline to say whether 1067 s is a regression since
then or has been the standing cost all along. **Establishing that is the first
step here**, and it is cheap: build a binary at `dev` around 2026-08-06 and run
the class alone with a large `--one-to`.

## What is known

* Not a JIT-mode artefact of the trap-replay guard: the guard refuses
  optimizing-tier bodies only where a trap would be unresumable, and this
  class's time is spent in the 10001-definition generate-and-compile path.
  An A/B with `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` is the one-command check
  and has not been run for *timing* (it was run for correctness, where the
  class fails outright).
* RSS reached ~6.1 GB during the long run, on a 31 GB host with no swap
  pressure — so this is not the 2026-08-02..05 heap-exhaustion shape, which
  died rather than finished.
* `TestContextAotGeneratorIntegrationTests` and
  `ApplicationContextAotGeneratorTests` complete inside the 180 s cap in the
  same run, so whatever this is, it is not Spring AOT generation generally.
  `AotIntegrationTests` sits between the two: 334 s alone (`OK`, 4 found /
  2 succ / 2 skip, matching HotSpot), inside the cap on a quiet host and over
  it under load. It is the same "a `TIMEOUT` row carries `found=0` and reads
  like a class that could not start" reporting problem, at a tenth the
  magnitude.

## Next steps

1. Fix the baseline: is 1067 s a regression, or the standing cost?
2. Profile the long run (`perf record` on the host) — the earlier
   `OldGen::scan_region_filtered` finding on this exact test came out of a
   profile, and its replacement is in `dev`, so the current hot term is
   unknown.
3. Decide whether the suite runner should carry a per-class timeout override
   for this class, so a full-suite run reports `OK` with a slow time rather
   than `TIMEOUT` with `found=0`. That is a reporting fix, not a VM fix, and
   should not be done before (1) — a raised cap would hide a future
   regression here completely.

## Reproducing

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'BeanRegistrationsAotContributionTests$' --tag brac \
  --one-to 2400 --batch-to 2700
JDK25=<jdk25> ./run-suite.sh hotspot \
  --only 'BeanRegistrationsAotContributionTests$' --tag brac-hs
```
