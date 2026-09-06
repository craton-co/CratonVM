# Hibernate and Hibernate Reactive — fails/hangs that are not CratonVM defects

**Status:** consolidates the two suites' known-non-bug residuals. **Re-verified
2026-08-24 on `dev@35bc2d5a7`** — every claim below was re-measured, not
carried forward. One entry is a genuine but open CratonVM performance
characteristic (not a correctness defect); the rest are host/environment
artifacts that reproduce identically under real HotSpot.

> ### What changed since the 2026-08-17 version
>
> The Hibernate Reactive half of this page was written when the residual was
> **seven** classes. It is now **three**, and the reasons the other four left are
> all recorded elsewhere:
>
> * `MultithreadedInsertionTest`, `MultithreadedIdentityGenerationTest` and
>   `it.LocalContextTest` no longer fail — the 61-class non-passed set was
>   re-run in full on 2026-08-24 and 14 of its 17 genuine failures now pass
>   (`internal/fixed-suite-bugs/hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md`
>   §10). Three fixes account for that: the `nio_selector` `SelectorImpl` field
>   corruption, the JIT lambda direct-call arm dropping a deoptimized frame, and
>   the `invokedynamic` trap.
> * `techempower.TechEmpowerTest` is **fixed**, and its old entry here was
>   wrong in kind: it was never a volume/timeout class. It returned a wrong
>   answer under the pre-bridge `invokedynamic` trap and passed with `--jit off`
>   in ~72 s, well inside its own budget. See
>   `internal/fixed-suite-bugs/jit/techempower-wrong-answer-was-the-indy-trap-FIXED-20260824.md`.
>
> **The "five are one open lambda-dispatch performance defect" framing in
> `residual-seven-after-the-afc-fix-20260817.md` should not be cited.** That
> page's own §5 records the prediction being A/B'd and failing to convert, and
> what actually closed four of the five was correctness work, not dispatch cost.

## Hibernate ORM

Verified on Azure Linux (`20.80.105.49`, JDK 25 Temurin), branch
`test/azure-recheck-fails-20260817` off `dev`, cross-checked on Windows the same
day. Full detail in
[hibernate-orm-hql-parser-memory-overhead-20260817.md](hibernate-orm-hql-parser-memory-overhead.md).

| Class | CratonVM | Real HotSpot | Verdict |
|---|---|---|---|
| `hql.HqlParserMemoryUsageTest` | FAIL: found=1 ok=0 failed=1, ms=41932 | PASS: found=1 ok=1 failed=0, ms=5433 | **Open, attributed CratonVM perf characteristic** — not a correctness bug |

**Re-measured 2026-08-24** on `dev@35bc2d5a7`, local Windows box, same
`common.args`, both VMs back to back — it still reproduces and is still
CratonVM-specific:

| | result | wall |
|---|---|---|
| CratonVM | **FAIL** `found=1 ok=0 failed=1` | 37.1 s |
| real HotSpot (JDK 25) | PASS `found=1 ok=1 failed=0` | 6.7 s |

Same shape as the 2026-08-17 figures (41.9 s / 5.4 s), so nothing about it has
drifted. This remains the one entry on this page that is a real CratonVM
characteristic rather than a host artifact, and it stays open —
[hibernate-orm-hql-parser-memory-overhead-20260817.md](hibernate-orm-hql-parser-memory-overhead.md)
is the detail.
| `annotations.uniqueconstraint.UniqueConstraintBatchingTest` | PASS | PASS | No longer reproduces |
| `query.hql.FunctionTests` | PASS (124/118/6skip) | PASS (124/118/6skip) | Host locale — see below |
| `query.hql.StandardFunctionTests` | PASS (44/44) | PASS (44/44) | Host locale — see below |

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

### The two `testFormat` rows are the HOST LOCALE, and the harness was missing the sysprop that pins it

**Measured 2026-08-29.** `FunctionTests.testFormat` and
`StandardFunctionTests.testFormat` came back red on the Windows host's H2
complete-suite run with

```
Expected: is "Monday, 25/03/1974"
     but: was "понедельник, 25/03/1974"
```

The A/B settles it in four runs on that host — the two VMs agree in **both**
arms:

| VM | `-Duser.language=en -Duser.country=US` | result |
|---|---|---|
| real HotSpot | no | **FAIL** — `"понедельник, 25/03/1974"` |
| real HotSpot | yes | PASS |
| CratonVM | no | **FAIL** — `"понедельник, 25/03/1974"` |
| CratonVM | yes | PASS |

hibernate-orm's own Gradle build sets that pair on every test JVM
(`local-build-plugins/src/main/groovy/local.java-module.gradle:260-261`), and
`apps/hib-suite-runner/common.args` carried `-Duser.timezone=UTC` but not the
locale half — so the harness was running the suite in a configuration upstream
never runs it in, on the one host whose OS locale is not English. The two rows
are now in the tracked-by-intent `required-sysprops.tsv` (`run-hib.sh sysprops`
reports `state: 4 (3 injected)`), which is the mechanism that exists precisely
because `common.args` is generated and untracked. Same family as the
`ru_RU` hibernate-validator row in the Hibernate Reactive section below.

No HANGs exist in the current default-collector Hibernate ORM full-suite
baseline (`FAIL=4→1, HANG=0, ABORTED=6` matching the known-benign self-skip
count).

### Two more from the 4548-class H2 run, both measured 2026-08-29

`UniqueConstraintBatchingTest.testBatching` is the SAME missing locale sysprop
as the two `testFormat` rows above, wearing a counter's clothes. Its assertion
is `assertEquals(1, triggerable.triggerMessages().size())` on a log watcher
built with `watchForLogMessages("Unique index")` — and **H2 localizes its own
`DbException` text from `Locale.getDefault()`**, so on this host the line reads
`Нарушение уникального индекса или первичного ключа: ...` and the watcher
matches nothing. The constraint violation itself happens correctly; the
`PersistenceException` catch block is what runs. Both VMs fail without
`-Duser.language=en -Duser.country=US` and pass with it. Fixed by the same two
`required-sysprops.tsv` rows.

`PackagedEntityManagerTest.testExcludeHbmPar` never ran against H2 at all. The
class boots an EMF from a JAR it builds out of
`hibernate-core/target/bundles/excludehbmpar/`, whose `persistence.xml` was
filtered for **PostgreSQL** at fixture-build time
(`jdbc:postgresql://localhost/hibernate_orm_test_$worker`). A persistence unit
carries its own connection settings, so the suite's H2 `hibernate.properties`
does not reach it. The `relation "caipirinha_seq" does not exist` wording said
so before any A/B — that is Postgres's sentence, not H2's. With no Postgres
reachable both VMs fail identically with
`PSQLException: Connection to localhost:5432 refused`.

Detail for both:
`internal/fixed-suite-bugs/hibernate/h2-complete-suite-misc-residuals-three-of-four-closed-20260829.md`.

## Hibernate Reactive

Full detail was in the now-retired
`residual-seven-after-the-afc-fix-20260817.md`,
filed the same day after a fix took the Windows FAIL bucket from 238 classes
to seven. Summarized here; nothing in the seven is a hang, a deadlock, or a
wrong answer.

### Two are the host, not the VM — fail identically under real HotSpot

**Both re-confirmed 2026-08-24** in the closing sweep, where they are now the
only two FAILs left in the entire 61-class non-passed set. Worth recording *how*
they were attributed, because the obvious method gets it wrong: the runner's
`shard-N/raw.log` is **cumulative for every class that shard ran**, so grepping
it for the timezone signature matches both classes and would misfile
`DatabaseHibernateReactiveTest` as a timezone failure. Attributed per class —
reading only the lines following each one's own `@@TESTFAIL` — they are
distinct, and match the table below: `ORMReactivePersistenceTest` fails with
`ServiceException … invalid value for parameter "TimeZone"`, while
`DatabaseHibernateReactiveTest` fails with an `AssertionError` in `nameIsNull`.

| Class | Cause |
|---|---|
| `ORMReactivePersistenceTest` | Windows host's `America/Buenos_Aires` time zone ID is the pre-2009 spelling; the `postgres:18.4` container's tzdata (no `backward` file) rejects it in the JDBC driver's startup packet. Verified identical under HotSpot and CratonVM (`../../../probes/DefaultLocaleTimeZoneProbe.java`), and both classes pass on the Azure host, which is UTC/`en`. |
| `it.quarkus.qe.database.DatabaseHibernateReactiveTest` | Windows host's `ru_RU` display language makes hibernate-validator resolve the Russian Bean Validation message instead of the English one the test asserts. Identical under HotSpot. |

### One is an open CratonVM performance residual — the other four are FIXED

**Rewritten 2026-08-24.** This section used to read "five are one open CratonVM
performance defect". Four of the five no longer fail, and the framing was wrong
about the fifth's neighbours, so it is replaced rather than amended.

| class | status 2026-08-24 |
|---|---|
| `MultithreadedInsertionTest` | **passes** |
| `MultithreadedIdentityGenerationTest` | **passes** |
| `it.LocalContextTest` | **passes** |
| `techempower.TechEmpowerTest` | **fixed** — and never a timeout class; it returned a WRONG ANSWER under the pre-bridge `invokedynamic` trap |
| `MultithreadedInsertionWithLazyConnectionTest` | **still open** — 1 of 2 methods |

The three that now pass were closed by correctness fixes, not by dispatch cost
going away: the `nio_selector` `SelectorImpl` field corruption and the JIT
lambda direct-call arm dropping a deoptimized frame. The closing sweep that
establishes this re-ran all 61 non-passed classes on current `dev`
(`internal/fixed-suite-bugs/hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md`
§10).

**The remaining one is genuinely a dispatch-cost residual**, and it is bounded
by the fixture's own hardcoded `@Timeout(10, MINUTES)` that no runner flag or
system property can reach — `testIdentityGenerator` passes,
`testIdentityGeneratorWithTransaction` does not. Checked on 2026-08-24 whether
the `invokedynamic` fix that retired TechEmpower also closes this: **it does
not**. Detail, including six dead hypotheses and a separate silent-insert-loss
finding, is in
[hib-reactive-multithreaded-insertion-lazy-connection-20260822.md](hib-reactive-multithreaded-insertion-lazy-connection-20260822.md).

The lambda-dispatch measurements the old text quoted (~1.7–2.1 µs per
functional-interface call, ~200–300x a static call) are still accurate as
measurements. What is retired is the claim that they *explain these classes* —
`residual-seven-after-the-afc-fix-20260817.md` §5 records that prediction being
A/B'd and failing to convert.

### `ProxyPreservingFiltersOutsideInitialSessionTest` — a `ConstraintViolationException`, confirmed harness/test-fragility, not a CratonVM defect (2026-09-05)

The 2026-09-05 Generational-GC hib-orm rerun
(`nonpassed-rerun-gen-20260905/run-20260905-182403-passed/on-real/shard-0/`)
showed:

```
MethodSource [className = 'org.hibernate.orm.test.filter.proxy.ProxyPreservingFiltersOutsideInitialSessionTest', methodName = 'testChangeFilterBeforeInitializeInSameSession', ...]
=> org.hibernate.exception.ConstraintViolationException: could not execute batch [Unique index or primary key violation: "PUBLIC.CONSTRAINT_C PRIMARY KEY ON PUBLIC.ACCOUNTGROUP(ID) ( /* key:1 */ CAST(1 AS BIGINT))"; ...]
```

That run's own startup line read `db-reset=off (--no-db-reset)` with a
`could not compile DbReset.java: worker-DB reset disabled` warning, raising
the obvious hypothesis: stale rows from an earlier run of the same class,
never cleaned up because the harness's worker-DB reset was disabled.

**Checked before writing anything, per this doc's own standard, and the
hypothesis does not hold, for two independent reasons:**

1. **`run-hib.sh`'s `db_reset`/`DbReset.java` mechanism only ever applies to
   MySQL/Postgres worker databases** (`WORKER_URL="jdbc:postgresql://..."` /
   `"jdbc:mysql://..."`, `run-hib.sh` lines ~515/522). This run used H2
   in-memory (`jdbc:h2:mem:db1;...;DB_CLOSE_ON_EXIT=FALSE`) — the reset
   machinery was never in the loop for this class regardless of the
   `--no-db-reset` flag. (`DbReset.java` itself was missing from
   `apps/hib-suite-runner/` entirely — recovered from an old worktree,
   `/data/cvm-devcheck/apps/hib-suite-runner/DbReset.java`, and copied back so
   the harness can compile it — but it would not have helped this failure
   even so.)
2. **It reproduces in full isolation**: single class, single shard, single
   fresh JVM, brand-new in-memory H2 database
   (`CV_BIN=<gen-wrapper> ./run-hib.sh --list <(echo ProxyPreservingFiltersOutsideInitialSessionTest) --shards 1`)
   — `found=4 started=2 ok=1 failed=1 skipped=2`, same
   `ConstraintViolationException`. There is no earlier run to have left stale
   rows.
3. **It reproduces identically on real HotSpot JDK 25**, same isolated
   single-class invocation
   (`java @common.args -Dcraton.batch=1 CratonRunner ...ProxyPreservingFiltersOutsideInitialSessionTest`):
   `found=4 started=2 ok=1 failed=1 aborted=0 skipped=2`, byte-for-byte the
   same counts and the same `ConstraintViolationException` on
   `AccountGroup(id=1)`.

**Mechanism (read from the test source, not further chased):** all four
`@Test` methods in this class hard-code `accountGroup.setId(1L)` and rely on
the schema being fresh per test method. `@SessionFactory` is declared at
class level; `SessionFactoryExtension` only implements
`TestInstancePostProcessor` + `BeforeEachCallback` (the latter is a no-op for
a class-level `@SessionFactory`) + `TestExecutionExceptionHandler` — no
`AfterEachCallback`, and `SessionFactoryScopeImpl` does not implement JUnit's
`Store.CloseableResource`, so nothing ever closes/drops one test method's
`SessionFactory` before the next method's `postProcessTestInstance` builds a
new one against the same underlying database. Whichever two of the four
`@Test` methods JUnit happens to execute first (no `@TestMethodOrder` is
declared, so method order is unspecified) collide if both insert
`AccountGroup(id=1)` without an intervening drop — exactly what `found=4
started=2 ok=1 failed=1` shows happening. This is a pre-existing fragility in
the test/harness combination (a single-class-per-JVM launcher without
Gradle's usual per-class isolation, paired with a Hibernate test class that
assumes but does not enforce inter-method schema isolation), reproducible on
both engines. **Not a CratonVM defect; no page needed beyond this entry.**

## Azure host note

The Azure host's own `hibernate-reactive-suite-runner/runs/` directory's most
recent entries (`nonpassed-{default,g1,zgc}-20260815`) are stale relative to
the above — a small 4-class rerun from 2026-08-15 where 2 of 4 classes failed
with `NO-DB: connection-refused` (a Docker/Postgres container was not up at run
time, not a VM result at all). The residual-seven investigation above is the
current, authoritative source for the Reactive suite; no fresh Azure run was
launched to produce this doc, per instruction.
