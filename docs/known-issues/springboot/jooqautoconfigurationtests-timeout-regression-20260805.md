# `JooqAutoConfigurationTests` — timeout, 2026-08-05

**Status: OPEN.** Two things this page originally got wrong, corrected
2026-08-05 (see "Correction" below): it is **not** a fresh 08-05 regression,
and it is **not** the dispatch bug that took out the four sibling
`*AutoConfigurationTests` classes filed the same day.

## Correction (2026-08-05, measured)

**1. It did not regress on 08-05.** Azure's own recorded rows:

| Run | Result | Seconds |
|---|---|---:|
| `craton-fullsuite-azure-20260802` | **HANG** | 300.153 |
| `craton-fullsuite-azure-20260805` | **HANG** | 300.042 |
| `hotspot-baseline-latest.tsv` | PASS | 17.918 |

It hung identically in the previous full suite. The "REGRESSED 2026-08-05"
framing below is true only against the 2026-07-18 closure (~4m20s), not
against the last full-suite comparison point.

**2. It is not the shared cause.** Four sibling classes filed the same day —
`Flyway`, `Jackson`, `Rabbit` and `TomcatServletWebServer`
`AutoConfigurationTests` — were all one bug, the recycled-`JitInvokeInfo`
dispatch aliasing fixed by `383e7f5cf` (see
`fixed-suite-bugs/springboot/flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`).
All four are green on the fixed binary. **This class is not:** on the same
binary, same host, same launcher it still HANGs — 900s on Azure and 1200s
locally. That makes it the negative control for that fix as well as its own
open problem.

**3. Where the time actually goes.** A local run on current dev
(`96acd76ed`, 1200s ceiling) reached only 9 of 17 `HikariPool` cycles.
Per-line deltas show the cost is **inside the test body**, between
`Start completed` and `Shutdown initiated`, not in pool setup or teardown:

```
   0.0s  21:45:53.605  HikariPool-1 - Start completed.
 138.6s  21:48:12.161  HikariPool-1 - Shutdown initiated...   <-- test body
   1.1s  21:48:13.238  HikariPool-2 - Starting...
   0.0s  21:48:13.276  Exception … Factory method 'settings' threw exception with message: null
   0.0s  21:48:13.278  HikariPool-2 - Shutdown initiated...   <-- failing context: 31ms
  90.6s  21:49:44.578  HikariPool-3 - Shutdown initiated...   <-- test body
 175.2s  21:52:42.001  HikariPool-4 - Shutdown initiated...   <-- test body
 158.8s  21:55:23.281  HikariPool-5 - Shutdown initiated...   <-- test body
```

So the original page's guess that the `Settings` NPE "or retrying around it"
was what stretched each cycle is **refuted**: contexts that hit that NPE cost
~31 ms. The expensive cycles are the ones that *succeed*, at 90-175s each
against ~1s/test on HotSpot (17 tests in 17.9s total).

**Two separable problems remain**, and they should not be conflated again:

* **(a) throughput** — a successful jOOQ test body costs 90-175s, ~100-175x
  HotSpot. This is what makes the class time out, and it is the whole of the
  timeout story.
* **(b) correctness** — `Failed to instantiate [org.jooq.conf.Settings]:
  Factory method 'settings' threw exception with message: null` still
  reproduces on the fixed binary. It is cheap and it is real, but it is not
  the timeout's cause.

## (a) is `Class.getDeclaredMethods()`, and it is not jOOQ-specific

Traced 2026-08-05, one step at a time, each step narrowing the last:

1. **One method reproduces it.** `jooqWithDefaultConnectionProvider` —
   6.0s HotSpot, **244.7s CratonVM (41x)** — and it executes **no SQL**, it
   only inspects beans. `noDataSource` (no `DSLContext` at all) is 2.4s vs
   3.0s, i.e. normal. So the cost is building the `DSLContext`, not using it.
2. **`--stack-sample-ms 100` over that method:** **1910 of 2042 samples
   (93.5%) under `EventListenerMethodProcessor`** →
   `MethodIntrospector.selectMethods` → `ReflectionUtils.doWithMethods` →
   one `AnnotatedElementUtils.findMergedAnnotation` per method.
3. **A probe of exactly that call** (`probes/AnnotationScanCacheProbe.java`,
   `MethodIntrospector.selectMethods` on `org.jooq.impl.DefaultDSLContext`,
   1003 declared methods): **457 ms HotSpot vs 151,695 ms CratonVM, 332x.**
   Spring's own cache buys HotSpot ~10x between passes and CratonVM ~1.5x.
4. **Not a lost cache.** Spring's `ConcurrentReferenceHashMap` (SOFT refs,
   the store behind `AnnotationsScanner`'s caches) retains perfectly on both:
   0 missing and 0 wrong of 200,000 entries. Hypothesis refuted.
5. **One native carries it** (`probes/ReflectionCacheProbe.java`):

   | call | HotSpot | CratonVM | ratio |
   |---|---:|---:|---:|
   | `Class.getDeclaredMethods()` | 46.9 µs | **3401.5 µs** | **73x** |
   | `Method.getParameterTypes()` | 0.102 µs | 0.262 µs | 2.6x |
   | `Method.getDeclaredAnnotations()` | 0.128 µs | 1.065 µs | 8.3x |

   The siblings are ordinary interpreter overhead. `getDeclaredMethods` is
   3.4 **milliseconds** a call, and Spring calls it once per type per
   hierarchy walk.
6. **Not descriptor work.** 1000 × `void m()` — no parameters at all — is
   still 91x (3711.9 µs vs 40.8 µs). Adding four reference parameters and a
   reference return costs only +23% (4580.0 µs). So descriptor→mirror
   resolution is not the term.
7. **Cost per method GROWS with method count** (Azure, `--Xmx 2g`, 100 calls):

   | methods | CratonVM µs/call | µs per method | vs previous | HotSpot |
   |---:|---:|---:|---:|---:|
   | 125 | 238.2 | 1.91 | — | 26.2 |
   | 250 | 482.5 | 1.93 | 2.03x | 71.4 |
   | 500 | 1319.9 | 2.64 | 2.74x | 70.7 |
   | 1000 | 3961.3 | 3.96 | 3.00x | 108.9 |

   Linear doubling would be 2.0x, quadratic 4.0x. The measured 2.03 → 2.74 →
   3.00 says **a large linear term (~1.9 µs/method) PLUS a super-linear term**
   that takes over as the class grows. At 1000 methods the two are roughly
   equal halves of the 3961 µs.

### Where to look, and one dead end already walked

Per mirror, `create_method_object` calls
`ctx.method_exceptions(declaring_class_id, &name, &descriptor)` and
`ctx.method_signature(…)`. Both are keyed by **name + descriptor**, which
implies a search of the declaring class's method table — O(n) inside a loop
that already runs n times. That is the most likely home of the super-linear
term and is the first thing to measure.

**Dead end (measured, do not repeat):** the same function also re-ran
`ensure_class_initialized`, `class_num_total_fields` and
`method_class_has_named_layout` per mirror, and wrote 13 fields **by name** —
~13,000 name→slot resolutions per call on a 1003-method class. Hoisting all of
that to once per native call (a `MethodMirrorLayout` resolved in the array
builders) looked obviously right and **measured as a wash**: interleaved
A-B-B-A-A-B on the Azure host, base 3618.0 / 3965.8 / 3864.3 µs vs fix
3423.4 / 3782.2 / 3654.9 µs — ~5% with the arms overlapping. It was not
landed, on this repo's own standard that a change carrying risk for no measured
throughput does not land. The by-name field writes are *not* the cost.

The shape that would actually close the gap is HotSpot's: cache the resolved
**root** `Method` mirrors per class and make each call a cheap copy (HotSpot
spends 41 ns per mirror doing exactly that). That is a redesign of a delicate
native — `setAccessible` state must stay per-copy, the GC pinning discipline in
`create_method_object` must survive, and the cache needs all three of
`site_cache.rs`'s validity conditions because `java/lang/reflect/Method` is
precisely the class Mockito's inline mock maker redefines. Not attempted here.

### This is not a jOOQ problem

`AnnotationsScanner.getBaseTypeMethods` calls `getDeclaredMethods()` for every
type it walks, so any class with a large method surface pays this. jOOQ's
`DefaultDSLContext` (1003 declared methods) is simply an extreme case. The same
native is the likely load-bearing term in the Tomcat annotation-scan wall
(224-259x) and the webapp-deploy wall (234x) already on file — worth re-testing
those against any fix here rather than treating this as one class's problem.

## Original regression note (2026-08-05), retained

`fixed-suite-bugs/springboot/jooq-destroy-method-ambiguity-and-hang-FIXED.md`
closed this exact class's "Cluster B" hang on 2026-07-18 (a Panama
downcall-adapter fallback reading field 0 of every invoke-shaped receiver,
fixed by requiring `java/lang/invoke/MethodHandle` before the adapter read),
reporting completion of all 17 tests in ~4m20s (JIT and `--nojit`) — inside
the 300s default budget, but with only ~40s of margin.

In today's 2026-08-05 full-suite rerun, `JooqAutoConfigurationTests` again
times out at the 300s budget (`TIMEOUT`/`HANG`; HotSpot baseline passes
cleanly, `PASS 0/17`). Given the narrow margin the 2026-07-18 fix left, and a
new intermediate symptom not present in that closure (below), this reads as
a regression of the same timeout, via a different proximate trigger.

## Symptom

`.out.log` shows the test class repeatedly building and tearing down
`HikariPool`-backed `DataSource`s (HSQLDB in-memory), each cycle taking
roughly **90-100 seconds** — far longer than a bean-creation-and-teardown
cycle should take:

```
11:49:24.183 [main] INFO ... HikariPool-1 - Added connection ...
11:51:02.665 [main] INFO ... HikariPool-2 - Starting...
11:51:02.703 [main] WARN ... AnnotationConfigApplicationContext -- Exception encountered during context initialization ...:
  Error creating bean with name 'settings' defined in
  org.springframework.boot.jooq.autoconfigure.JooqAutoConfiguration:
  Failed to instantiate [org.jooq.conf.Settings]: Factory method 'settings'
  threw exception with message: null
11:51:03.234 [main] INFO ... HikariPool-3 - Starting...
   (~97s later)
11:52:40.926 [main] INFO ... HikariPool-3 - Shutdown initiated...
11:52:42.510 [main] INFO ... HikariPool-4 - Starting...
   (~93s later)
11:54:15.165 [main] INFO ... HikariPool-4 - Shutdown initiated...
11:54:15.624 [main] INFO ... HikariPool-5 - Starting...
```

The process is killed at the 300.042s budget shortly after `HikariPool-5`
starts — i.e. it was still making forward progress (not a total stall like
`FlywayAutoConfigurationTests` in this same rerun), just at roughly one
`@Test`/pool-cycle per 90-100s against 17 total tests, which cannot finish
in 300s.

## Root cause

**Not confirmed — needs further investigation.** New lead not present in the
2026-07-18 closure: `Failed to instantiate [org.jooq.conf.Settings]:
Factory method 'settings' threw exception with message: null` — an
unwrapped NPE with no message, thrown while building jOOQ's `Settings` bean
(this class is JAXB-annotated; the original Cluster B hang was also JAXB/
Panama-adjacent). Worth checking whether this NPE, or retrying/re-resolving
around it, is what stretches each pool cycle to ~90-100s — that magnitude
does not match any known HikariCP or Spring default timeout, so it likely
reflects the same interpreted/reflection-heavy dispatch cost documented for
other "severe slowdown" classes (e.g.
`jacksonautoconfigurationtests-severe-slowdown-FIXED-20260722.md`'s
`update_root_snapshot` O(stack-depth) mechanism) rather than a fixed sleep.

## Affected classes

- `module/spring-boot-jooq` — `org.springframework.boot.jooq.autoconfigure.JooqAutoConfigurationTests`

Log: `craton-fullsuite-azure-20260805-s6/all-jit/logs/module_spring-boot-jooq.org.springframework.boot.jooq.autoconfigure.JooqAutoConfigurationTests.{out,err}.log`
