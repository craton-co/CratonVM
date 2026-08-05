# `FlywayAutoConfigurationTests` — silent hang after HSQLDB `DbValidate`, 2026-08-05

**Status: OPEN — found 2026-08-05**

## Symptom

`module/spring-boot-flyway`'s `FlywayAutoConfigurationTests` times out at the
suite's 300s per-class budget (`TIMEOUT`/`HANG`, `rc` never returned). HotSpot
baseline passes the same class cleanly (`hotspot-baseline: PASS 0/73`).

The `.out.log` shows completely normal progress through several embedded-DB
migration cycles (H2, twice), then starts an HSQLDB-backed Flyway executor:

```
11:39:50.836 [main] INFO org.flywaydb.core.FlywayExecutor -- Database: jdbc:hsqldb:mem:f888540c-1003-4450-aef9-73609953d35f (HSQL Database Engine 2.7)
11:39:51.091 [main] INFO ... Schema history table "PUBLIC"."flyway_schema_history" does not exist yet
11:39:51.096 [main] INFO org.flywaydb.core.internal.command.DbValidate -- Successfully validated 0 migrations (execution time 00:00.152s)
11:39:51.098 [main] WARN org.flywaydb.core.internal.command.DbValidate -- No migrations found. Are your locations set up correctly?
```

That is the **last line of output**. The process was started at 11:38:23 and
killed at the 300s timeout (~11:43:23); nothing further is written to
stdout or stderr for the remaining ~212 seconds — no GC-fallback warnings,
no JIT log lines, no further Flyway/JUnit progress at all, unlike the
GC-fallback-spam pattern seen in `ConfigurationPropertySourcesTests` and
`ZipContentTests` in this same rerun. This total silence (not merely slow
progress) points at a genuine stall rather than a throughput problem.

## Root cause

**Not confirmed — needs further investigation.** This is a different class
of failure from the two other Flyway docs already on file for this exact
class:

- `flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md` (fixed 2026-07-12) —
  a fatal SIGSEGV from a heap-header misread plus an HSQLDB JIT crash (fixed
  by keeping `org/hsqldb/` interpreted). That fix is why this run's HSQLDB
  path is running interpreted, but it does not explain a silent stall.
- `jooq-destroy-method-ambiguity-and-hang-FIXED.md` Cluster A (fixed
  2026-07-18, `Class.getMethods()` hierarchy merge) — a `Cannot resolve
  method 'shutdown' to a unique method` `BeanCreationException`, which
  produces a clean `FAIL` with a stack trace, not a silent hang. Today's run
  produces no such exception at all before going silent.

The stall begins immediately after Flyway's `DbValidate` reports zero
migrations for the (empty-locations) HSQLDB case — plausibly during the
subsequent embedded-database shutdown, or in whatever bean/context step runs
next for that particular `@ParameterizedTest`/`@Test` configuration. No
thread/stack dump was captured for this run (`--stack-dump-on-timeout` was
not enabled), so there is no direct evidence of where the thread is stuck.

## Affected classes

- `module/spring-boot-flyway` — `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests`

Log: `craton-fullsuite-azure-20260805-s5/all-jit/logs/module_spring-boot-flyway.org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests.{out,err}.log`
