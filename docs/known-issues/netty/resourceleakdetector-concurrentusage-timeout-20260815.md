# `ResourceLeakDetectorTest.testConcurrentUsage` blows its 60 s timeout on every collector

**Status: OPEN**, characterised 2026-08-15. Split out of the retired
`zgc-resourceleakdetector-corpse-read` write-up, whose closing pass found that
the residual it had recorded as "GC-independent and belongs to whoever owns
that test" was only half right.

## What it is

```
@@TESTFAIL io.netty.util.ResourceLeakDetectorTest testConcurrentUsage() FAILED
@@RESULT io.netty.util.ResourceLeakDetectorTest found=3 started=3 ok=2 failed=1 ...
```

`testConcurrentUsage` carries `@Timeout(value = 60000, unit = MILLISECONDS)`.
The class reports 61.3–65.3 s. Nothing else in it fails.

## It is not the collector, and it is not the test's owner

15 interleaved runs, one class per VM, five reps of each arm:

| arm | reps | outcome | class wall time |
|---|---|---|---|
| ZGC, JIT on | 5 | `ok=2 failed=1` | 62.5–64.6 s |
| ZGC, `--nojit` | 5 | `ok=2 failed=1` | 61.6–65.3 s |
| G1 | 5 | `ok=2 failed=1` | 61.3–61.6 s |

Same test fails in every one. **HotSpot 25 passes it**, in 2.8 s for the whole
class — and fails the other two (`testLeakBrokenHint`, `testLeakSetupHints`,
both leak-report assertions, 3/3 runs). So the two VMs fail *disjoint* sets,
and CratonVM's `ok=2 failed=1` against HotSpot's `ok=1 failed=2` is not the
improvement the arithmetic suggests. Reading a raw pass count as a comparison
here gets the sign wrong.

## The shape of the workload

`testConcurrentUsage` starts **50 threads**, each looping up to 1000 times, each
iteration allocating 100 `DefaultResource` + `ResourceLeakTracker` +
`LeakAwareResource` triples into a per-thread `ArrayDeque` and then closing them
all. `DefaultResource.detector` is a shared `ResourceLeakDetector`, and
`DefaultResourceLeak extends WeakReference`, so every iteration also registers
and clears reference-processor entries.

That makes at least four candidate costs, none of them yet separated:

* **thread count.** 50 mutators on an 8-core host, with a global
  `ref_processor` mutex (L7) taken on every `Reference` construction and every
  `SoftReference.get()`.
* **the reference processor itself.** Registration is O(1) but the pre-GC null
  pass and the post-GC restore are O(active references), and this workload
  holds up to 5000 of them at once.
* **`CyclicBarrier` / `AtomicBoolean` contention**, i.e. the ordinary
  interpreter-vs-JIT gap on a lock-dense loop.
* **JIT reach.** The arms above say the JIT does not help here at all: ZGC with
  the JIT and ZGC with `--nojit` are the same to within noise, which for a
  1e6-allocation loop is itself the finding worth chasing first.

## Traps for whoever picks this up

* **The `@Timeout` hides whether it is slow or hung.** 61–65 s against a 60 s
  budget is one or two seconds over; it could equally be a workload that would
  finish at 70 s or one that would never finish. Raise the budget before
  measuring anything else — `-Djunit.jupiter.execution.timeout.default` does
  not override a method-level `@Timeout`, so this needs an edited fixture or a
  standalone driver.
* **The host is shared.** These numbers were taken at load average 35–55 on 8
  cores. That inflates a 50-thread test far more than a single-threaded one, so
  the absolute wall times are an upper bound and the *ratio* to a HotSpot run
  taken at a different moment is not trustworthy. Re-measure both arms
  back-to-back before quoting a factor.
* **Do not read the other two tests' results as CratonVM being ahead.** They
  fail on HotSpot for a reason unrelated to this VM (leak-report
  cross-contamination between tests sharing one JVM), and they pass here for a
  reason nobody has established. A passing test whose oracle fails is a
  question, not a credit.

## Repro

```bash
cd apps/netty-suite-runner
<cratonvm> --java-home <jdk-25> --Xmx 1500m -XX:+UseG1GC @common.args \
  CratonRunner io.netty.util.ResourceLeakDetectorTest
```

Oracle: `java @common.args CratonRunner io.netty.util.ResourceLeakDetectorTest`.
