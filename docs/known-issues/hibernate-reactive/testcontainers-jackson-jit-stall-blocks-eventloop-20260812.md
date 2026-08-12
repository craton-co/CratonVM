# Testcontainers/Jackson deserialization stalls the Vert.x event loop on CratonVM — blows `before()` setup timeouts

**Status:** OPEN (2026-08-12). Found on Windows (`C:\craton\CratonVM`) during a
non-passed-class rerun of the hibernate-reactive suite (238 classes, 2 GC
variants — default/ZGC, 8 shards each), binary built from an isolated
worktree merged fresh to `origin/dev` (commit `0fe5fc997`).

## Symptom

179/238 classes (75%) fail — identically across both GC variants (`HANG=15
NOTESTS=44 FAIL=179` in both), so this is GC-independent. The dominant
signature across the FAIL bucket (315 occurrences across the run):

```
java.util.concurrent.TimeoutException: before(io.vertx.junit5.VertxTestContext) timed out after 120 seconds
```

i.e. the test class's `@BeforeEach`/`before()` setup — which provisions a
Testcontainers Postgres instance and opens a reactive session — never
finishes within Vert.x JUnit5's own 120s internal budget. Everything
downstream cascades from this: 327 `NullPointerException`s
(`SessionFactoryManager.getHibernateSessionFactory()` returns null because
setup never completed), 227 `VertxException: Thread blocked` warnings, 44
"test execution timed out" (a second, execution-level Vert.x timeout).

## Root cause: CratonVM stalls the Vert.x event-loop thread during Testcontainers/Jackson work

Isolated via the raw per-class logs. During container-startup and Docker-API
interaction (Testcontainers uses Jackson to deserialize Docker API JSON
responses), CratonVM's own logs show repeated JIT bailouts on the exact
same Jackson method:

```
cratonvm_jit::x64::driver: JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method="org/testcontainers/shaded/com/fasterxml/jackson/databind/deser/BeanDeserializerBase._resolveManagedReferenceProperty:(...)..."
```

This runs on Vert.x's event-loop thread (Testcontainers' container-start
call happens synchronously inside the reactive session-setup path here),
and while it's slow, Vert.x's own `BlockedThreadChecker` fires repeatedly:

```
WARNING io.vertx.core.impl.BlockedThreadChecker  Thread vert.x-eventloop-thread-0 has been blocked for 3982 ms, time limit is 2000 ms
WARNING io.vertx.core.impl.BlockedThreadChecker  Thread vert.x-eventloop-thread-0 has been blocked for 10056 ms, time limit is 2000 ms
... (climbing, one warning per second, for the duration of the stall)
```

The event loop stays blocked long enough (in aggregate, across the whole
container-start + Jackson-deserialize path) that the outer 120s `before()`
budget is exceeded. This is a **throughput problem, not a functional
correctness bug** — the operations eventually would succeed, they're just
too slow on CratonVM to fit inside a timeout that HotSpot clears
comfortably.

## HotSpot-clean confirmation

Ran the identical class (`CachedQueryResultsGenerateStatisticsTest`), same
classpath, under stock HotSpot: **PASS, wall_seconds=14, ms=11567**. The
identical class on CratonVM didn't even complete `before()` inside its
120s allowance — well over 8x slower, likely much more (the class never
reached the point HotSpot needed only ~11.5s to reach). Genuine
CratonVM-specific performance defect.

## Scope beyond hibernate-reactive

The exact same JIT-bailout signature on the exact same Jackson method
(`BeanDeserializerBase._resolveManagedReferenceProperty`) was also seen
independently in a netty smoke-test run the same day (unrelated app,
same day's session) — Testcontainers/Jackson interaction is not
hibernate-reactive-specific. Any CratonVM workload that spins up
Testcontainers (or otherwise deserializes large/complex JSON via Jackson
on a latency-sensitive thread) should be assumed at risk of the same
stall. Worth checking whether the JIT bailout itself (code-buffer-size
misestimation forcing a bail-and-retry) is the primary cost, or whether
it's just a visible symptom of broader interpreter/JIT throughput
overhead on this call shape.

## Impact

Turns 179 of 238 non-passed hibernate-reactive classes into `FAIL` via
timeout rather than a clean pass or a clear functional defect — this
likely significantly overstates how many of these classes have "real"
CratonVM correctness bugs versus "CratonVM is currently too slow to fit
inside this suite's timeouts." Worth revisiting the FAIL bucket after this
performance gap narrows, since an unknown fraction of the 315
`before()`-timeout occurrences may resolve to PASS on their own once
Testcontainers/Jackson startup is fast enough.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
echo org.hibernate.reactive.CachedQueryResultsGenerateStatisticsTest > /tmp/one.txt
export TESTCONTAINERS_RYUK_DISABLED=true   # see jna-native-clinit-nativeversion-npe / Ryuk-connectivity note below
CV_BIN=bin/cratonvm-hibreactive-default.exe bash run-hibernate-reactive-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 240 --out /tmp/repro
# HotSpot cross-check:
bash run-hibernate-reactive-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```
Deterministic in the sense that the class reliably exceeds the 120s
`before()` budget on CratonVM; exact wall-clock varies with host load
(this run had 16 concurrent CratonVM forks + Postgres containers active).

## Related

- On this same Windows machine, Testcontainers' default `Ryuk` reaper
  could not be reached from CratonVM (`Could not connect to Ryuk at
  localhost:<port>`), separate from the JNA `Native.<clinit>` NPE
  documented for the Azure run
  (`docs/known-issues/hibernate-reactive/jna-native-clinit-nativeversion-npe-20260812.md`).
  Not yet root-caused as CratonVM-specific vs. a Windows/Docker-Desktop
  networking quirk — worked around here with
  `TESTCONTAINERS_RYUK_DISABLED=true`, which is what let this run produce
  real per-class results instead of a uniform Ryuk-connection failure.
  Disabling Ryuk means Testcontainers-started Postgres containers are
  never auto-removed; this rerun used a periodic external sweep script
  (`cleanup-sweep.sh`, removes any `postgres:18.4` container older than
  360s) to avoid exhausting local Docker Desktop resources — same
  mitigation the Azure run's report recommended for next time.
- `docs/known-issues/hibernate-reactive/vertx-pg-sasl-scram-handshake-fails-20260812.md`
  — the dominant blocker on the Azure host's PostgreSQL run was a SASL/SCRAM
  protocol-violation error, essentially never seen in this Windows rerun's
  logs (only 1 occurrence found across the full 238-class × 2-variant run).
  The two runs hit different dominant failure modes on the same suite —
  not yet reconciled; could be environment-dependent (different Postgres
  container config/version defaults between the two Testcontainers setups)
  or timing-dependent (the Azure run's SASL error happened very early in
  session setup, before this Windows run's classes get far enough to hit
  it, because they're stalling earlier still in Testcontainers/Jackson).
