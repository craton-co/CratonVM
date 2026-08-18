# `Log4J2LoggingSystemTests` / `LogbackLoggingSystemTests` — 300s HANG

**Status: RETIRED — 2026-08-10** (branch `fix/logging-systemtests-classpath-hang-20260810`).

Supersedes `docs/known-issues/springboot/log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`.
The `Log4J2LoggingSystemTests` half is now covered by
[`retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md`](../../../known-issues/retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md).

The retired doc was careful and explicit that its root cause was a hypothesis.
It ran the three experiments it named as missing, and the answers split the two
classes apart.

## The measurement

Standalone, one class at a time, `--Xmx 2g`, `--stack-sample-ms 2000`, HotSpot
control on the same host and classpath. Binary `cratonvm-logsys-20260810.exe`
(dev `6365de194`).

| Class | HotSpot | CratonVM JIT | CratonVM `--nojit` | fallback peak |
|---|---:|---|---:|---:|
| `Log4J2LoggingSystemTests` | 10.7s (61, 14 fail) | **no completion in 3600s** | **293.3s** (61, 14 fail) | **#4096** |
| `LogbackLoggingSystemTests` | 8.8s ✓86/86 | **268.2s** ✓86/86 | 277.5s ✓86/86 | none |

Two different problems:

- **`Log4J2LoggingSystemTests` is the `[moving-young]` fallback death spiral**,
  the same defect as `Integration`/`Quartz`. It never finishes under the JIT and
  completes in 293.3s under `--nojit`. Its 14 failures are identical on HotSpot,
  so they are environmental and not part of this.
- **`LogbackLoggingSystemTests` is not.** It completes both ways with zero
  fallbacks and a clean 86/86. It is simply **30.5x** HotSpot (268.2s vs 8.8s),
  which puts it just under the 300s line — a genuine margin case, and the only
  one of the two the retired doc's framing actually fits.

### The trap this class set

`Log4J2LoggingSystemTests`'s 300s suite log contains **zero** fallback lines,
which is why it was first read as *not* an instance of the GC defect. Given a
3600s budget the same class reaches **#4096**. The escalation outlasts the
budget that kills the process, so a timed-out log cannot be used to rule the
fallback out. That is now recorded in the live doc's banner.

## The doc's own hypothesis, tested

> the working hypothesis is that on CratonVM each pass costs enough more than on
> HotSpot … that the **cumulative** cost across dozens of test methods in one
> class exceeds the 300s per-class budget

**Confirmed for `Logback`, with the hot terms named.** Deepest-frame profile,
133 samples, JIT-on, no fallback (so unconfounded):

| Group | Share |
|---|---:|
| Annotation discovery — `AnnotationUtils.findAnnotation`, `isAnnotated`, `$Proxy*.annotationType` | 22.6% |
| Reflective method scan/sort — `ReflectionUtils.streamMethods`, `Method.equals`, `findAllMethodsInHierarchy`, `defaultMethodSorter` | 23.3% |
| `ModifiedClassPathClassLoader.loadClass` | 6.8% |

`Log4J2` under `--nojit` (146 samples) agrees: annotation discovery 28%, the
`String.equals`/`startsWith` comparisons underneath it 16%, `loadClass` 10%.

So the per-method nested `Launcher.discover()+execute()` is indeed the cost, and
within it the dominant terms are **annotation scanning and reflective method
enumeration/sorting** — consistent with the already-measured reflection gap
(`Method.getModifiers()` at 39 µs/call vs HotSpot's 1 ns, from the retired jOOQ
write-up). It is **not** classloader construction:
`ModifiedClassPathClassLoader.get` is cached in a `ConcurrentReferenceHashMap`,
and what is hot is `loadClass`, not `compute`.

### A profile that had to be thrown away

The first profile of `Log4J2` was taken from its JIT run — which was inside the
fallback spiral — and showed `ConcurrentReferenceHashMap` construction
(`createReferenceManager`, `Segment.<init>`, `createReferenceArray`, plus its
`ReentrantLock`/`ReferenceQueue`) at ~22%, the largest single group. Re-derived
from the `--nojit` run those frames **vanish entirely**. They were the degraded
allocator showing up in allocation-heavy code, not a real cost. Profile a class
that is in the fallback state and you measure the fallback.

## Candidate 1 (GC amplification): right, and for both reasons

The doc suspected the `[moving-young]` fallback but could not commit, because
`LogbackLoggingSystemTests` showed the warning and `Log4J2LoggingSystemTests`
showed none. That asymmetry was real but inverted by budget: with a full budget
`Log4J2` is the one with #4096, and `Logback` — whose 300s log showed peak
**#2** — never escalates at all.

## Candidate 2 (`@ClassPathOverrides` / Aether): right conclusion, wrong reason

The doc dismissed Aether for `Log4J2` on the grounds that:

> This cannot explain `Log4J2LoggingSystemTests`'s stall — it has no
> `@ClassPathOverrides` at all, only `@ClassPathExclusions`.

**It does have one.** `Log4J2LoggingSystemTests` carries
`@ConfigureClasspathToPreferLog4j2`, a composed annotation that itself declares
`@ClassPathOverrides({"org.apache.logging.log4j:log4j-core:2.24.3", …})`.
`getAdditionalUrls` resolves via `MergedAnnotations`, so meta-annotations count.
The profile confirms it: Aether's `DefaultRepositorySystem.resolveDependencies`
appears at depth 78 under
`ModifiedClassPathClassLoader.get → compute → processUrls → getAdditionalUrls →
resolveCoordinates`.

The conclusion still holds, but on measured grounds rather than assumed ones:
Aether accounts for **6%** of samples in the `Log4J2` `--nojit` run and 2–3%
elsewhere. Real, worth knowing, nowhere near enough to explain a 174s blackout.

## What remains open

- **`LogbackLoggingSystemTests` at 30.5x HotSpot.** Not a defect with a
  reproduction, but the reason it sits one bad run away from the timeout. The
  named terms above are where that 30x lives.
- **The 174.4s single blackout** in the original 2026-08-06 `Log4J2` log
  (02:09:36.444 → 02:12:30.853, resuming in `customExceptionConversionWord`).
  The fallback spiral explains why that run never finished; whether one
  contiguous three-minute stall is the spiral's onset or something else is not
  separately established.
- **The 14 `Log4J2` failures**, identical on both VMs, are environmental to this
  host and were not investigated.
