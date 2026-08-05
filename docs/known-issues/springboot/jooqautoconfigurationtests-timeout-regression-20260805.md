# `JooqAutoConfigurationTests` — timeout REGRESSED, 2026-08-05

**Status: OPEN — REGRESSED 2026-08-05.**

## Regression note (2026-08-05)

`docs/internal/fixed-suite-bugs/springboot/jooq-destroy-method-ambiguity-and-hang-FIXED.md`
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
