# Hibernate and Hibernate Reactive — fails/hangs that are not CratonVM defects

**Status:** consolidates the two suites' known-non-bug residuals as of `dev@090416c56`
(2026-08-17). One entry is a genuine but open CratonVM performance characteristic
(not a correctness defect); the rest are host/environment artifacts that
reproduce identically under real HotSpot.

## Hibernate ORM

Verified on Azure Linux (`20.80.105.49`, JDK 25 Temurin), branch
`test/azure-recheck-fails-20260817` off `dev`, cross-checked on Windows the same
day. Full detail in
[hibernate-orm-hql-parser-memory-overhead-20260817.md](hibernate-orm-hql-parser-memory-overhead-20260817.md).

| Class | CratonVM | Real HotSpot | Verdict |
|---|---|---|---|
| `hql.HqlParserMemoryUsageTest` | FAIL: found=1 ok=0 failed=1, ms=41932 | PASS: found=1 ok=1 failed=0, ms=5433 | **Open, attributed CratonVM perf characteristic** — not a correctness bug |
| `annotations.uniqueconstraint.UniqueConstraintBatchingTest` | PASS | PASS | No longer reproduces |
| `query.hql.FunctionTests` | PASS (124/118/6skip) | PASS (124/118/6skip) | No longer reproduces |
| `query.hql.StandardFunctionTests` | PASS (44/44) | PASS (44/44) | No longer reproduces |

`HqlParserMemoryUsageTest` (regression test for upstream `HHH-19240`) asserts a
single cold HQL parse allocates under 256 MiB. A standalone probe replicating
Hibernate's `StandardHqlTranslator.parseHql()` exactly shows ANTLR's SLL
prediction mode succeeds on **both** VMs — no fallback to the expensive LL
parse on either, ruling out a dispatch/exception-handling divergence. CratonVM
allocates ~1.8–1.9x more heap garbage than HotSpot for that one cold parse, on
both Windows and Azure Linux; warm repeats are cheap on both VMs, so it is not
a leak. This is a real, reproducible, platform-independent interpreter
allocation-volume gap (likely ANTLR's SLL ATN-configuration/DFA-state
construction), not a wrong answer and not a measurement artifact — CratonVM
has no allocation-site profiler yet to pin down the exact multiplier's source,
so root-causing it further is left open.

The other three classes were flagged earlier this session on Windows as FAILs
that also reproduced under real HotSpot there. Rechecked here on a second,
independent platform against the current `dev` tip, all three now pass cleanly
on both VMs with byte-for-byte matching `found/ok/failed/skipped` counts.
`dev` moves fast; whatever caused the earlier Windows failures has already
been resolved by unrelated fixes merged since. They are not currently known
issues.

No HANGs exist in the current default-collector Hibernate ORM full-suite
baseline (`FAIL=4→1, HANG=0, ABORTED=6` matching the known-benign self-skip
count).

## Hibernate Reactive

Full detail in
[residual-seven-after-the-afc-fix-20260817.md](hibernate-reactive/residual-seven-after-the-afc-fix-20260817.md),
filed the same day after a fix took the Windows FAIL bucket from 238 classes
to seven. Summarized here; nothing in the seven is a hang, a deadlock, or a
wrong answer.

### Two are the host, not the VM — fail identically under real HotSpot

| Class | Cause |
|---|---|
| `ORMReactivePersistenceTest` | Windows host's `America/Buenos_Aires` time zone ID is the pre-2009 spelling; the `postgres:18.4` container's tzdata (no `backward` file) rejects it in the JDBC driver's startup packet. Verified identical under HotSpot and CratonVM (`probes/DefaultLocaleTimeZoneProbe.java`), and both classes pass on the Azure host, which is UTC/`en`. |
| `it.quarkus.qe.database.DatabaseHibernateReactiveTest` | Windows host's `ru_RU` display language makes hibernate-validator resolve the Russian Bean Validation message instead of the English one the test asserts. Identical under HotSpot. |

### Five are one open CratonVM performance defect, not a correctness bug

`MultithreadedInsertionTest`, `MultithreadedIdentityGenerationTest`,
`MultithreadedInsertionWithLazyConnectionTest`, `it.LocalContextTest`,
`techempower.TechEmpowerTest` — all volume-driven (thousands of inserts or
HTTP round trips), all correct when given enough time (raising only the
timeout, two pass outright at 11.9x and 21.2x HotSpot's wall time; the other
three get much further than the suite lets them before hitting the fixture's
own hardcoded Vert.x deadline, which no runner flag can reach).

Root cause, measured and reproduced on both Windows and Azure Linux: invoking
a lambda/functional-interface method costs CratonVM ~1.7–2.1 µs — 8–10x
HotSpot's *interpreter*, while a plain static call on CratonVM is 2–4x
*faster* than HotSpot's interpreter (own control probe,
`probes/CompositionPrimitivesProbe.java`). HotSpot's interpreter puts a lambda
call at ~11–13x a static call; CratonVM puts it at ~200–300x. hibernate-reactive's
`CompletableFuture`/`AsyncTrampoline`/Vert.x `Handler` pipeline is built almost
entirely out of functional-interface invocations, which is why exactly these
five classes (and no others) are left. A `perf record` profile is flat — no
single hot body, ~11% in lambda-dispatch-named frames — consistent with a
structural per-call cost (an `RwLock` read plus a `HashMap` lookup on the
process-global `classes.lambda_proxies` map, up to twice per invocation) rather
than one fixable hot spot. This page deliberately stops at the measurement and
does not prescribe a fix; any candidate must be A/B'd on
`MultithreadedInsertionTest`'s wall clock, not the microbenchmark.

## Azure host note

The Azure host's own `hibernate-reactive-suite-runner/runs/` directory's most
recent entries (`nonpassed-{default,g1,zgc}-20260815`) are stale relative to
the above — a small 4-class rerun from 2026-08-15 where 2 of 4 classes failed
with `NO-DB: connection-refused` (a Docker/Postgres container was not up at run
time, not a VM result at all). The residual-seven investigation above is the
current, authoritative source for the Reactive suite; no fresh Azure run was
launched to produce this doc, per instruction.
