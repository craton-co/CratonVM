# Crashes and hangs across the 2026-09-21/22 full-suite correctness sweep

Cross-cutting page: every hang/crash-shaped finding (a class that neither passed nor cleanly failed — it took down its own process, or was killed silent-at-the-wall) found across this session's class-by-class correctness sweep of spring, tomcat, h2, spring-boot, hibernate (ORM), bouncy castle, and apache commons math, run at the project's current defaults (`--jdk-only`-as-default, no GC/JIT overrides, default timeouts). Per-app FAIL censuses live in each app's own `docs/known-issues/<app>/` page; this page exists so a hang/crash isn't only visible from inside a much longer FAIL list.

**Status as of this writing: 4 of 7 apps covered.** Tomcat, H2, and Spring ran on the Azure host, which went completely unreachable (TCP-level, not just SSH — see below) partway through writing these pages. Their crash/hang rows are pending and will be appended here once the host is reachable again. Do not read their absence as "no crashes found" — it means "not yet checked."

## Hibernate ORM — 2 CRASH, 2 HANG

Local Windows run, commit `6989206e5`, `C:/craton/CVM/target/release/cratonvm.exe`, real JDK 25, all defaults. Full detail: [hibernate/nonpassed-classbyclass-census-20260922.md](hibernate/nonpassed-classbyclass-census-20260922.md).

| class | kind | rc | timeout |
|---|---|---|---|
| `org.hibernate.orm.test.softdelete.collections.MappingTests` | CRASH | 127 | 300s |
| `org.hibernate.orm.test.type.temporal.InstantTests` | CRASH | 127 | 300s |
| `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | HANG | 124 | 3600s (ran the FULL hour before being killed) |
| `org.hibernate.orm.test.type.temporal.LocalDateTimeTest` | HANG | 124 | 300s |

`rc=127` for the two CRASH rows is not a typical native-crash exit code (SIGSEGV is usually `139`, SIGABRT `134`) — this needs the actual process log checked before it's confirmed as a real VM crash rather than a harness/classpath artifact. Note `org.hibernate.orm.test.type.temporal` accounts for 2 of these 4 rows (plus 3 more FAILs in the same package, in the per-app census) — the single biggest cluster in the whole Hibernate ORM sweep.

## Bouncy Castle (bc-java) — 1 CV-BROKEN (hang, not scored CRASH but the same shape)

Local Windows run, commit `6989206e5`, same binary. Full detail: [bc-java/nonpassed-census-20260922.md](bc-java/nonpassed-census-20260922.md).

| class | verdict | evidence |
|---|---|---|
| `org.bouncycastle.crypto.test.AllTests` | CV-TIMEOUT-STALLED (CV-BROKEN) | killed at 600s cap after **599s of total silence** (HotSpot: 153.2s to completion) |

The corpus harness's own convention reads this as "very often a SIGSEGV or a livelock that printed no result line" — genuinely crash/hang-shaped even though the corpus driver's verdict vocabulary doesn't use the word CRASH. Not yet reproduced standalone.

## Spring Boot — 1 HANG (pre-existing, not part of the 61-class regression)

Local Windows run. Full detail: [springboot/nonpassed-classbyclass-census-20260922.md](springboot/nonpassed-classbyclass-census-20260922.md). The suite's `all-jit` summary carries `CRASH=24` and `HANG=1` — both counts are unchanged from the 2026-09-13 baseline (the diff analysis in that page found only PASS→FAIL movement, 0 movement in CRASH/HANG), so none of these 25 rows are new to this sweep. Not enumerated here since they predate this run; see the suite's own historical known-issues pages for that 24-class CRASH cluster.

## Netty — sweep incomplete, separate finding already surfaced live

Netty's class-by-class run from earlier this session was abandoned (90-minute timeout at 19/657 classes, superseded before this sweep). A **batched** (all-classes-in-one-invocation) re-run was launched on Azure during this session and stalled again: `@@RESULT` count flat at 19/733 for 35+ minutes while the process burned ~115% CPU in `io.netty.buffer.PooledByteBufAllocatorTest` (following a class that itself had 28/49 sub-tests abort), with a `junit-timeout-t*` guard thread spinning at 96.6% CPU and zero log output — a spin, not a clean deadlock. The user approved killing that process and resuming over the remaining testlist; this was blocked by the same Azure outage described below and is still pending. Netty is not otherwise represented in this census — no per-class correctness data exists yet.

## Pending: Tomcat, H2, Spring (Azure)

The Azure host (`azureuser@20.80.105.49`) went unreachable at the TCP level (not just SSH auth/banner — a raw `/dev/tcp` connect to port 22 also hangs and times out) partway through this work, immediately after Netty's batched run had been sustaining 100%+ CPU for over an hour. Whether that's cause and effect or coincidence isn't established. From memory of this session's earlier (pre-outage) results:

- **Tomcat**: class-by-class sweep scored PASS=589 FAIL=24 HANG=26 (639/640) — notably worse than this project's existing `tomcat/nonpassed-class-census.md` baseline (624/640 from 2026-08-23). The 26 HANG classes are not yet enumerated by name in this document; they need pulling from `apps/tomcat/.suite/results/classbyclass-default-20260922/shard-0/results.csv` once Azure is reachable.
- **H2**: class-by-class sweep scored PASS=166 HANG=14 FAIL=38 (218) — worse than the existing `h2/nonpassed-40-census-20260818.md` baseline. 14 HANG classes seen by name over the course of the run (`TestOpenClose`, `TestScript`, `TestBenchmark`, `TestMVStoreBenchmark`, `TestBtreeIndex`, `TestPowerOffFs`, `TestSimpleIndex`, `TestTools`, and others) but the final authoritative list has not been pulled from the results file.
- **Spring**: class-by-class sweep scored classes: LOADERR=9 OK=2801 FAIL=37 TIMEOUT=1 (test-methods: found=30683 passed=30055 failed=457). No CRASH-specific breakout has been pulled yet.

This page will be updated with named classes for all three once the host is reachable again.
