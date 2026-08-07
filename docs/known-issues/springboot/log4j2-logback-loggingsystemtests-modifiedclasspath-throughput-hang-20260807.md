# `Log4J2LoggingSystemTests` / `LogbackLoggingSystemTests` HANG — class-level `@ClassPathExclusions` forces every test method through `ModifiedClassPathExtension`'s nested-Launcher re-execution, cumulative cost exceeds the 300s class timeout

**Status: OPEN — new finding 2026-08-07. Confirmed NOT a recurrence of the
2026-07-17/19 `ModifiedClassPathExtension` recursion livelock (that bug's own
signature is absent here, and its fix is verified still in effect).**

## Symptom (2026-08-06 full-suite Windows run, `craton-fullsuite-windows-20260806-s1`)

| Class | Status | Seconds |
|---|---|---:|
| `core/spring-boot` `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` | HANG | 300.175 |
| `core/spring-boot` `org.springframework.boot.logging.logback.LogbackLoggingSystemTests` | HANG | 300.071 |

Both processes are killed at the shard's 300s per-class timeout with no
`SBRUNNER_RESULT` line. Neither log shows a crash, an assertion failure, or
an unhandled exception — both show **steady partial progress** (dozens of
"Hello world" test-body log lines accumulate over the run) interrupted by
one or more multi-minute stretches of total silence.

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s1/all-jit/logs/core_spring-boot.org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests.{out,err}.log`,
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s1/all-jit/logs/core_spring-boot.org.springframework.boot.logging.logback.LogbackLoggingSystemTests.{out,err}.log`.

### `Log4J2LoggingSystemTests` timeline

Process starts ~02:08:02. Test-body output ("Hello world" etc.) appears at a
roughly 2-15s cadence from 02:08:16 through 02:09:36 (~14 test methods'
worth), **then 2m54s of total silence** (stdout and stderr both — no GC
messages, no partial output) from 02:09:36 to 02:12:30, when the
`customExceptionConversionWord` test's own `logger.warn("Expected
exception", ...)` call appears (this is the test's own deliberate log
output, not an anomaly — see
`apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/logging/log4j2/Log4J2LoggingSystemTests.java:344`).
Two more test methods complete after that, ending at 02:12:58.810 — 4
seconds before the timeout kills the process at 02:13:02.

### `LogbackLoggingSystemTests` timeline

Process starts ~02:15:16. Test-body output at similar cadence (2-10s between
lines) from 02:15:26 through 02:16:26.258 (the last line in `.out.log`).
`.err.log` shows two `cratonvm_gc::gc_quiescence` `[moving-young] fallback`
warnings — `#1` at 02:17:36 (`reason=innermost-rbp-belongs-to-unguarded-callee`)
and `#2` at 02:19:42, same reason — i.e. **some background GC activity
continued for ~4 more minutes past the last test-body output**, then total
silence until the 300s timeout kills the process (~02:20:16).

## What this is not

- **Not the 2026-07-17/19 `ModifiedClassPathExtension` recursion livelock**
  ([`modifiedclasspath-aether-network-hang-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/modifiedclasspath-aether-network-hang-cluster-FIXED.md),
  [`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)).
  That bug's signature was a **tight, steady (~1-2s cadence) repeat of the
  same `cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read
  dropped` warning against `InterceptingExecutableInvoker`/`InvocationInterceptorChain`
  objects**, for the entire process lifetime, with **zero** JUnit test output
  ever appearing (`isModifiedClassPathClassLoader`'s classloader-identity
  check never tripped, so `interceptMethod` re-entered a fresh nested
  `Launcher.execute()` forever — `StackOverflowError` when fast, HANG when
  each pass was slow). Neither log here shows that warning at all, and both
  logs show substantial, varied real test output (dozens of distinct test
  bodies' log lines) accumulating throughout the run — the opposite of "zero
  JUnit output." The 2026-07-19 fix (isolated-loader class-identity
  resolution in `classloader.rs`/`lang_system.rs`/`interpreter.rs`) is
  confirmed still in effect for these two classes.
- **Not a classic deadlock either**, in the sense of two threads permanently
  blocked on each other — both processes visibly complete dozens of distinct
  test methods (with distinct log output, distinct assertions, distinct
  `CapturedOutput` content) before running out of the 300s budget. This is
  forward progress that's simply too slow, not a stuck state, for at least
  the majority of the run.

## Root cause — hypothesis, not fully confirmed

Both classes carry **class-level** `@ClassPathExclusions`:

- `Log4J2LoggingSystemTests`: `@ClassPathExclusions("logback-*.jar")` (line 101, ~61 `@Test` methods)
- `LogbackLoggingSystemTests`: `@ClassPathExclusions({ "log4j-core-*.jar", "log4j-api-*.jar" })` (line 112, ~86 `@Test` methods), plus 2 methods additionally carrying `@ClassPathOverrides` (Maven coordinate resolution via Eclipse Aether)

A class-level annotation means **every** `@BeforeEach`/`@Test`/`@AfterEach`
invocation for the whole class goes through
`ModifiedClassPathExtension.interceptMethod`/`intercept`
(`apps/spring-boot/test-support/spring-boot-test-support/src/main/java/org/springframework/boot/testsupport/classpath/ModifiedClassPathExtension.java`),
which — once the now-fixed recursion guard correctly detects "already inside
the isolated classloader" — still does, **once per method, every method**:

```java
private void runTest(String testId) throws Throwable {
    LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
        .selectors(DiscoverySelectors.selectUniqueId(testId)).build();
    Launcher launcher = LauncherFactory.create();
    TestPlan testPlan = launcher.discover(request);
    ...
    launcher.execute(testPlan);
    ...
}
```

i.e. build/reuse a `ModifiedClassPathClassLoader`, then run a **complete,
brand-new JUnit Platform discovery + execution pass** for that single test,
in-process. This is architecturally expensive on any JVM; the working
hypothesis is that on CratonVM each pass costs enough more than on HotSpot
(consistent with this repo's independently-documented, unrelated-but-similar
findings that reflective/classloading-heavy operations run tens to hundreds
of times slower here — see
the retired
`jooqautoconfigurationtests-timeout-getmodifiers-FIXED-20260807` write-up,
whose measured term is `Method.getModifiers()` at 39 us/call against
HotSpot's 1 ns — NOT `Class.getDeclaredMethods()`, which that page originally
named and which measured 13 ms of a 65.6 s pass; and
[`!springboot-ldap-dsa-tls-windows-only-gap.md`](!springboot-ldap-dsa-tls-windows-only-gap.md)
for a differently-shaped but similarly Windows-specific gap) that the
**cumulative** cost across dozens of test methods in one class exceeds the
300s per-class budget, even though **no single method** hangs forever.

This does not by itself explain the multi-minute *silent* stretches (a
uniformly-slower-per-method cost would look like steadily lengthening gaps
between log lines, not one abrupt 2-3 minute blackout followed by a return
to normal cadence). Two candidate explanations for the blackout, neither
confirmed:

1. **GC-side amplification.** `LogbackLoggingSystemTests`'s `.err.log` shows
   the `[moving-young] fallback: reason=innermost-rbp-belongs-to-unguarded-callee`
   warning appearing during its silent stretch — the same warning and reason
   already implicated in a **separate, independently-filed** throughput-wall
   regression for Tomcat
   ([`gc-moving-young-persistent-nonmoving-fallback-regression.md`](../tomcat/gc-moving-young-persistent-nonmoving-fallback-regression.md)),
   where persistent moving-young→non-moving-sweep fallback was measured
   making a workload 40-80x slower than its closed baseline while still
   making genuine (just very slow) forward progress. If the same fallback is
   active here, an allocation-heavy operation (building a whole new
   `URLClassLoader` + re-running JUnit discovery/execution machinery,
   repeated ~60-85 times per class) landing on the degraded non-moving
   allocator would plausibly produce exactly this "long quiet stretch, GC
   warnings during it, then resumes" shape. **Not confirmed**: `Log4J2LoggingSystemTests`'s
   silent stretch shows *no* GC fallback warning at all in its `.err.log`
   (only 7 lines total, none during the gap), so if GC is the mechanism for
   `LogbackLoggingSystemTests` it is not obviously the same mechanism for
   `Log4J2LoggingSystemTests` — or the fallback simply wasn't logged (e.g. it
   only logs on state transition, not every occurrence) for the latter.
2. **`@ClassPathOverrides` network resolution**, for `LogbackLoggingSystemTests`'s
   two annotated methods only (lines 166, 174) — `ModifiedClassPathClassLoader`'s
   `getAdditionalUrls` calls Eclipse Aether's `DefaultArtifactResolver` when
   `@ClassPathOverrides` is present (confirmed mechanism, per the historical
   doc's "Update 2026-07-19" section, though that doc found the *network*
   angle was a red herring for the classes it investigated and the real bug
   was the recursion guard). Whether Aether resolves from a local `.m2`
   cache instantly or attempts a live network call on this host is not
   checked in this pass. This cannot explain `Log4J2LoggingSystemTests`'s
   stall — it has no `@ClassPathOverrides` at all, only `@ClassPathExclusions`.

## What would confirm this

- `--stack-sample-ms` (or `--stack-dump-on-timeout`) attached to a standalone
  rerun of either class, to see what the main thread is doing during one of
  the silent stretches — the single strongest missing piece of evidence.
- A timing breakdown per test method (log line deltas, same technique as
  `jooqautoconfigurationtests-timeout-regression-20260805.md`'s "Where the
  time actually goes" section) to see whether cost is flat-per-method (points
  at the nested-Launcher-per-method overhead itself) or concentrated in one
  or two methods (points at something method-specific, e.g. the
  `@ClassPathOverrides` Aether resolution).
- Standalone rerun of `Log4J2LoggingSystemTests`/`LogbackLoggingSystemTests`
  with `CRATONVM_MOVING_YOUNG=0` to check whether forcing the non-moving
  allocator (or, conversely, avoiding the fallback) changes the timing —
  same lever the Tomcat GC-fallback doc proposes and has not yet run either.

## Affected classes (this run)

| Module | Class | Trigger annotation |
|---|---|---|
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` | class-level `@ClassPathExclusions("logback-*.jar")` |
| `core/spring-boot` | `org.springframework.boot.logging.logback.LogbackLoggingSystemTests` | class-level `@ClassPathExclusions({"log4j-core-*.jar","log4j-api-*.jar"})` + 2 methods with `@ClassPathOverrides` |
