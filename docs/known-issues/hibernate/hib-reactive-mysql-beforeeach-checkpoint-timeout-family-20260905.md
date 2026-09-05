# hibernate-reactive on MySQL — one `VertxTestContext` checkpoint-timeout signature accounts for ~21 of 23-25 FAILs, GC-independent, root cause not yet isolated

## Status

**OPEN, newly characterized 2026-09-05.** Not found under `docs/known-issues/`
or `docs/internal/` by class name, by `VertxTestContext`/checkpoint-timeout
text, or by "mysql" (the closest relative, a stale-schema cross-class
cascade in the **Hibernate ORM** classic suite's own MySQL runner, was
checked directly and ruled out as the cause here — see below). This page
reports the shape and rules out two candidate causes; it does not pin the
defect.

**Severity:** MEDIUM — ~21/249 classes (8%) FAIL identically on two of three
GC arms with the same test-infrastructure-level symptom, none of them sharing
an obvious feature area (entity mapping, soft-delete, mutation-delegate,
locking, and more all affected identically).

## Run this was found in

Local Windows box, fresh isolated `hibernate-reactive` binary (built
~09:51 local time 2026-09-05), MySQL via Testcontainers (one container **per
class** — confirmed fresh per class below, not shared), `passed`-category
249-class suite, 3 shards, one run per GC arm:

| arm (GC) | PASS | FAIL | HANG | NOTESTS | wall |
|---|---:|---:|---:|---:|---:|
| `mysql-default` (ZGC) | 180 | **23** | 1 | 45 | 66m17s |
| `mysql-generational` | 178 | **25** | 1 | 45 | 68m7s |
| `mysql-g1` | (run still in progress at triage time — not used for this page) | | | | |

`results.tsv` at
`apps/hibernate-reactive-suite-runner/runs/mysql-{default,generational}-20260905-3gc-mysql-local/run-20260905-095151-passed/on-real/shard-{0,1,2}/results.tsv`.

## The FAIL set is 92% identical across GC arms

22 of the default arm's 23 FAIL classes also FAIL on generational (96%); only
`NonNullableManyToOneTest` is default-only. Generational adds 3 of its own:
`MultithreadedInsertionTest`, `it.DirtyCheckingIT`, `it.LocalContextTest`.
This overlap is why the signature below is reported as one family rather than
23-25 separate investigations.

## The shared signature

With three named exceptions (below), every FAIL class's `raw.log` shows the
identical exception, differing only in which test method(s) hit it and how
many:

```
java.util.concurrent.TimeoutException: The test execution timed out. Make sure your asynchronous code includes calls to either VertxTestContext#completeNow(), VertxTestContext#failNow() or Checkpoint#flag()

Unsatisfied checkpoints diagnostics:
-> checkpoint at io.vertx.junit5.VertxExtension.lambda$testContext$1(VertxExtension.java:175)
	at io.vertx.junit5.VertxExtension.joinActiveTestContexts(VertxExtension.java:321)
	at io.vertx.junit5.VertxExtension.joinActiveTestContexts(VertxExtension.java:257)
	at io.vertx.junit5.VertxExtension.interceptBeforeEachMethod(VertxExtension.java:222)
	...
	at org.junit.jupiter.engine.extension.TimeoutExtension.interceptBeforeEachMethod(TimeoutExtension.java:78)
	...
```

i.e. the harness's own `@BeforeEach` interception (`VertxExtension`'s own
per-test-context join, wrapped by JUnit's `TimeoutExtension`) times out before
the test method body ever runs — the checkpoint is never satisfied during
setup, not during the test's own assertions. Most affected classes fail every
one of their test methods this way (`found == failed`, `ok == 0`); a few
(`EmbeddedIdWithManyEagerTest` 2 ok/1 failed, `SoftDeleteSingleTableTest` 5/1,
`SoftDeleteTablePerClassTest` 3/1) show the identical exception on only one
method, confirming this is a per-test-method event, not always a whole-class
one.

Representative classes (spanning entirely unrelated feature areas —
collections, embedded ids, soft-delete, mutation-delegate, locking, identity
generation, batching): `CollectionStatelessSessionListenerTest`,
`EmbeddedIdWithManyTest`, `EmbeddedIdWithManyEagerTest`,
`EmbeddedIdWithOneToOneTest`, `LockTimeoutTest`,
`ManyToOneMapsIdAndEmbeddedIdTest`, `MultithreadedIdentityGenerationTest`,
`MutationDelegateIdentityTest`, `MutationDelegateJoinedInheritanceTest`,
`MutationDelegateTest`, `NoEntitiesTest`, `NoLiveTransactionValidationErrorTest`,
`NonNullableManyToOneTest`, `RowIdUpdateAndDeleteTest`,
`SoftDeleteCollectionTest`, `SoftDeleteConverterTest`, `SoftDeleteJoinedTest`,
`SoftDeleteSingleTableTest`, `SoftDeleteTablePerClassTest`,
`IdentityGenerationWithBatchingTest`, `it.DirtyCheckingIT`.

**Three classes in the same FAIL union are explicitly NOT part of this
family** (different, already-characterized causes):
* `techempower.TechEmpowerTest` — the already-known lambda/`invokedynamic`
  dispatch family (`docs/known-issues/hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`).
* `it.quarkus.qe.database.DatabaseHibernateReactiveTest` — the already-known
  Windows-host `ru_RU` locale/hibernate-validator message mismatch (same doc).
* `MultithreadedInsertionTest` — its own `@Timeout`-annotated test method
  times out at its own **540-second** budget (`TimeoutExceptionFactory`, a
  different exception shape entirely), matching the already-documented
  `CompletableFuture`/lambda-dispatch cost family in
  `docs/known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`'s
  sibling class, not the checkpoint-timeout shape above.

`NoLiveTransactionValidationErrorTest`'s presence here is worth flagging: it
was previously named in
`docs/internal/fixed-suite-bugs/hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md`'s
opening section as one of three classes with "the ZGC-only `@BeforeEach`
timeouts ... not reproduced in isolation ... look like Testcontainers/Docker
resource contention under 6-way concurrent shard load," explicitly left
uninvestigated. This run shows the identical class failing the identical way
on **both** ZGC (`default`) and Generational — i.e. **not ZGC-only** as that
page's tentative framing guessed, and reproducible at 3-way (not 6-way)
sharding. This page's 21-class family is a much larger, more systematic
version of that same, previously-parked observation.

## Two candidate causes checked and ruled out

**Not a stale cross-class schema leak.** The Hibernate ORM classic suite has
a well-documented MySQL-specific defect of this shape
(`docs/internal/fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`,
fixed in that suite's own runner): a killed/timed-out class leaks its schema
into a shared worker database, poisoning later classes with mismatched
tables. The hibernate-reactive suite runner's own comment
(`hibfix-3gc-mysql.sh`: "one Testcontainers container per class, which is
what gives per-class schema isolation") claims this cannot happen here, and
direct log inspection confirms it: every class's log shows a brand-new
container creation with a distinct ephemeral port
(`Container ... started in PT30.1s ... JDBC URL: jdbc:mysql://localhost:64449/hreact`,
next class: `:51443/hreact`, etc.) — never a reused container. The
`Table 'hreact.X' doesn't exist` / `DDL command failed [... drop foreign
key ...]` `WARN` lines that appear in every class's log (including passing
ones) are Hibernate's own `hbm2ddl.auto=create` issuing a defensive DROP
against a table that, on a genuinely fresh container, does not exist yet —
expected, harmless chatter on first boot, not evidence of leftover state from
a prior class. This rules out the ORM suite's mechanism as an explanation
here.

**Not container-startup latency alone.** Sampled container-start times are
~30-32 seconds, well under the 240-second per-class harness timeout, and
container creation itself never appears adjacent to a failing checkpoint in
the logs checked — the timeout fires during the test framework's own
`VertxExtension`/`TimeoutExtension` interception, after the container and
`SessionFactory` are already up (schema-creation DDL for the class in
question is visible completing before the failing test method's own
`@@TESTFAIL` line in every sampled case).

## What is NOT established

The actual mechanism inside `VertxExtension.joinActiveTestContexts`/
`interceptBeforeEachMethod` that fails to complete is not identified. Given
the breadth of unrelated feature areas affected identically, a
CratonVM-side reactive-dispatch or Vert.x-event-loop timing issue (in the
same general family as the already-extensively-documented
`CompletableFuture`/lambda composition cost problems in
`docs/internal/fixed-suite-bugs/hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md`)
is a plausible next hypothesis, but this session did not instrument
`VertxExtension` itself, did not run any affected class in isolation to check
whether it reproduces outside the full-suite/shard context, and did not
A/B `--jit off` (the lever that cleanly separated JIT-caused correctness
issues from environment issues in the related persistence-context-cascade
investigation cited above).

## A same-mechanism data point from the (incomplete) G1 arm

The G1 arm's shard-2 (still the only shard recorded at triage time) shows a
**HANG** for `MultithreadedIdentityGenerationTest` at a 600-second timeout —
that class FAILs (not hangs) with the checkpoint-timeout signature above on
both `default` and `generational`. A single occurrence in an unfinished arm
is not enough to call this the same defect manifesting more severely under
G1, but it is consistent with that and worth re-checking once the G1 arm
completes.

## Reproduction

```
apps/hibernate-reactive-suite-runner/runs/mysql-default-20260905-3gc-mysql-local/run-20260905-095151-passed/on-real/shard-{0,1,2}/raw.log
apps/hibernate-reactive-suite-runner/runs/mysql-generational-20260905-3gc-mysql-local/run-20260905-095151-passed/on-real/shard-{0,1,2}/raw.log
```
grep any of these for `@@TESTFAIL` blocks followed by `The test execution
timed out` to see the family; cross-reference `results.tsv`'s `ok`/`failed`
columns (`awk -F'\t' 'NR>1 && $3=="FAIL"{print $2,$4,$5,$6}' results.tsv`).

## Related

* `docs/internal/fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`
  — the superficially similar, already-fixed ORM-suite defect; checked and
  ruled out as this page's cause.
* `docs/internal/fixed-suite-bugs/hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md`
  — its opening section's "ZGC-only `@BeforeEach` timeouts ... not yet
  reproduced in isolation" note is the small, tentative seed this page grows
  from; that framing (ZGC-only) does not hold given this run's Generational
  data.
* `docs/known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`
  — the separate, already well-documented dispatch-cost defect that
  `MultithreadedInsertionTest`/`MultithreadedInsertionWithLazyConnectionTest`
  belong to instead of this family.
* `docs/known-issues/hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`
  — covers `TechEmpowerTest` and `DatabaseHibernateReactiveTest`, excluded
  from this family above.
