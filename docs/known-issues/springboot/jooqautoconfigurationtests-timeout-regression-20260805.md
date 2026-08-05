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
