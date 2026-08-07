# H2 — MVStore insert/commit loop performance cliff (timeout "hang")

## Status
**OPEN** — performance, not a deadlock. Tracked as a hang because it exceeds any
practical per-class timeout.

**Misfiled note (2026-08-07):** this doc was sitting under
`docs/internal/fixed-suite-bugs/h2-suite-bugs/` despite an OPEN status —
against the project's own `docs/known-issues` convention (OPEN bugs live in
`docs/known-issues/`; only FIXED ones move to `docs/internal/`). Restored to
`docs/known-issues/h2/` here. No content below this line and the "Confirmed
instance" sections was changed by the move itself.

## Severity
**HIGH (throughput)** — likely the dominant cause of the CratonVM-specific
"silent hangs" in the sweep (db / mvcc / store classes that loop over many
row operations).

## Representative class
`org.h2.test.db.TestTempTables` (`testAnalyzeReuseObjectId`): a single
connection runs
```java
for (int i = 0; i < 10000; i++) prep.execute();   // insert default values, autocommit
```
HotSpot completes the whole class in ~2 s. CratonVM exceeds 180 s (≈ ≥90×
slower **for this workload**, versus the ~5× average on passing classes).

## Diagnosis
Watchdog stack dump (`CRATONVM_DEFAULT_WATCHDOG_SEC=45`) sampled the single
stuck `JdbcPreparedStatement.execute` 707 times. The frames are **not** parked
in a lock wait — they vary across samples through the normal insert+commit
machinery, i.e. the thread is *progressing, just far too slowly*:

```
JdbcPreparedStatement.execute → Command.executeUpdate → CommandContainer.update
  → Insert.update → Insert.insertRows → MVTable.addRow → MVPrimaryIndex.add
  → MVMap.operate / TransactionMap.putIfAbsent / TransactionMap.set
SessionLocal.commit → Transaction.commit → TransactionStore.commit → MVMap.operate
Command.stop, SessionLocal.hashCode, Page.clone, Long.<init>
```

Each of the 10 000 iterations performs a full autocommit transaction:
`TransactionMap.putIfAbsent` over an `MVMap`, then `TransactionStore.commit`
(another `MVMap.operate`), plus `Page.clone` and boxing (`Long.<init>`,
`SessionLocal.hashCode`). The per-row transaction-commit machinery over
CratonVM's MVMap/ConcurrentHashMap path is the hot spot.

(An earlier, narrower dump caught `MVTable.lock`/`doLock1`/`doLock2`/`unlockAll`
frames and looked like a lock livelock; the wider sample shows those are just
the per-row lock acquire/release on the normal path, not a stuck wait.)

## Another confirmed instance (2026-07-31)

`org.h2.test.db.TestOutOfMemory` — once its `SIGABRT` was fixed (see
`bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted-FIXED.md`) the
class stopped crashing and started *timing out* instead. A
`--stack-dump-on-timeout 420` run on the **unmodified baseline** puts the main
thread in exactly this shape: `CreateTable.insertAsData -> Insert.insertRows ->
Select$LazyResultQueryFlat.fetchNextRow -> StringFunction1.getValue`, i.e. the
`create table ... as select x, space(1000000+x) from system_range(1, 10000)`
insert loop, progressing (frames differ between samples). HotSpot runs the whole
class in 5s.

## Why it matters
H2's TestAll exercises many large loops (bulk insert, fuzz, random ops). The
per-operation overhead in MVMap/transaction commit turns these from seconds into
minutes, so they trip the suite timeout and read as hangs.

## Suspected hot spots (for follow-up profiling)
- `org.h2.mvstore.tx.TransactionMap.putIfAbsent` / `set` → `MVMap.operate`
- `org.h2.mvstore.tx.TransactionStore.commit` → `MVMap.operate`
- `org.h2.mvstore.Page.clone` (copy-on-write page churn → allocation/GC churn)
- Boxing churn (`Long.<init>`, `SessionLocal.hashCode`)

## Next steps
- Sampling profiler (`cdb` / CRATONVM JIT scan-cache recipe) over a 10 000-row
  insert microbench to find the dominant cost (interpreter dispatch vs GC vs
  CHM/MVMap).
- Confirm which of the 30 silent-hang classes are this perf cliff vs genuine
  multi-thread deadlocks (e.g. `TestMvccMultiThreaded*`) by stack-dumping each.

## Affected (candidate) hang classes
TestTempTables, TestAnalyzeTableTx, TestIndex, TestFullText, TestCompatibility,
TestCancel, TestGetGeneratedKeys, TestCachedQueryResults, TestNestedLoop,
TestMvccMultiThreaded(2), TestBtreeIndex, TestCrashAPI, TestFuzzOptimizations,
TestLimit, TestNestedJoins, TestMultiThread, TestOptimizations, TestCompress,
TestFreeSpace, TestKillProcessWhileWriting, TestCacheLongKeyLIRS, TestSpinLock,
TestScript … (to be partitioned perf-vs-deadlock).


## Confirmed instance (2026-08-07): `TestCases`

Full-suite rerun on a clean, near-idle host (`origin/dev` merge @
`f9315411a`, load average ~14) put `org.h2.test.db.TestCases` in the
41-class non-passing set as a plain 900s HANG — not yet on the "candidate"
list above, but the exact same mechanism.

`--stack-dump-on-timeout 30` (direct invocation, bypassing the suite
runner — see note below) on the unmodified fixed binary:

```
tid=0 depth=0  TestCases.main
tid=0 depth=1  TestBase.testFromMain
tid=0 depth=2  TestCases.test
tid=0 depth=3  TestCases.testReuseSpace
tid=0 depth=4  JdbcStatement.execute
tid=0 depth=5  JdbcStatement.executeInternal
tid=0 depth=6  Command.executeQuery
tid=0 depth=7  CommandContainer.query
tid=0 depth=8  ScriptCommand.query
tid=0 depth=9  ScriptCommand.generateInsertValues
tid=0 depth=10 ValueVarchar.getSQL
tid=0 depth=11 StringUtils.quoteStringSQL
tid=0 depth=12 StringUtils.quoteIdentifierOrLiteral
```

`testReuseSpace` drives H2's `SCRIPT` command
(`ScriptCommand.generateInsertValues`), which walks every row of a table and
emits a quoted SQL literal per column — a large loop over
per-character/per-value string-building work, structurally identical to the
`TransactionMap`/`MVMap` per-row cost already characterized above, just
reached through the SQL dump path instead of the insert path. Same shape:
progressing (frames vary sample to sample, not parked on a lock), just too
slow for any practical timeout.

Move `TestCases` from "candidate" to **confirmed**.

**Diagnostic note:** `CRATONVM_DEFAULT_WATCHDOG_SEC` set through
`run-h2-suite.sh`'s `env VAR=... ./run-h2-suite.sh` wrapper did not produce a
watchdog dump in the per-class log (the class's log file had only the
startup banner, no `T19.H1` output, even past the watchdog deadline and
before the harness's own external timeout killed it) — the mechanism by
which the env var fails to reach the child through that wrapper is
unconfirmed and worth a follow-up. Invoking the binary directly
(`env CRATONVM_DEFAULT_WATCHDOG_SEC=N <bin> ... <class>`, bypassing the
runner script entirely) works reliably and is what produced the dump above.


## Further confirmed instances (2026-08-07), same signature: row/result iteration too slow, not parked

All four via `--stack-dump-on-timeout 25` direct-binary invocations (see the
runner-wrapper caveat in the `TestCases` section above — same technique).
Each main-thread dump shows the thread actively inside SQL execution over a
large row/statement set, not blocked on a lock:

* **`TestSubqueryPerformanceOnLazyExecutionMode`** — a performance test by
  name. Stuck in `Select.queryGroup → gatherGroup → TableFilter.next →
  IndexCursor.next → LazyResult.hasNext`, i.e. lazy-execution row-by-row
  grouping. Exactly the row-iteration cost this doc already characterizes.
* **`TestScript`** — H2's own giant built-in `.sql` script-file runner
  (thousands of statements). Stuck in the same
  `Select.queryGroup → gatherGroup → TableFilter.next → IndexCursor.next →
  Value.cache` chain. Already listed as "candidate" above; now confirmed.
* **`TestCrashAPI`** — internally drives `TestScript.getAllStatements`
  against the same corpus. Stuck in `Select.gatherGroup →
  SelectGroups$Grouped.nextSource → SessionLocal.compare` — row-comparison
  cost inside grouping/sorting over the same large script.
* **`TestOpenClose`** (`testBackupWithYoungDeadChunks`) — a parser-side
  instance of the same family rather than an execution-side one: stuck in
  `Parser.parseInsert → parseValuesForCommand → ParserBase.checkLiterals`,
  i.e. **parsing** (not executing) a large multi-row `INSERT ... VALUES`
  literal list. Confirms the throughput cliff is not confined to
  MVMap/TransactionMap commit cost — the SQL parser's per-literal overhead
  scales the same way over a big statement.

Move all four from "candidate" to **confirmed**.

## Three more HANGs with a DIFFERENT stuck locus (same sweep, needs separate follow-up)

Three classes from the same batch do **not** show the row-iteration
signature above — flagged here rather than folded in, since a single
watchdog sample can't yet prove "progressing slowly" for these the way the
repeated multi-sample dumps did for the classes above. Written up as their
own record:
[`bug-h2-hang-cluster-lirs-trace-mvstore-compact-20260807.md`](bug-h2-hang-cluster-lirs-trace-mvstore-compact-20260807.md)
— `TestLIRSMemoryConsumption` (stuck in `CacheLongKeyLIRS` eviction),
`TestBtreeIndex` (stuck in `Trace.isEnabled`, i.e. inside H2's own debug-code
tracing, not query execution), `TestSynth` (stuck in `MVStore.close() →
compactStore()`, i.e. close-time compaction, not live query work).


## Final batch confirmed (2026-08-07): `TestKill`, `TestPowerOffFs`, `TestPowerOffFs2`, `TestCancel`, `TestPerfectHash`

Same technique, same sweep. All fit the established signature:

* **`TestKill`** — `Select.gatherGroup → updateAgg → DataAnalysisOperation
  .updateAggregate → AbstractAggregate/Aggregate.updateAggregate →
  ExpressionColumn.getValue`: aggregate computation over a large result set
  in `checkData`. Same grouping-cost family as `TestScript`/`TestCrashAPI`
  above.
* **`TestPowerOffFs`** — caught mid-`MVStore.commit → TransactionStore
  .endTransaction → Transaction.commit/close`, i.e. squarely the
  transaction-commit path this doc's representative example
  (`TestTempTables`) already names.
* **`TestPowerOffFs2`** — same `testCrash` → `JdbcStatement.execute` shape
  as `TestPowerOffFs`; capture was cut short by the log-extraction window
  (31 total frames, only the outer ones shown here) but the entry shape
  matches the same family. Worth a follow-up full capture if this one is
  ever prioritized individually.
* **`TestCancel`** — `Select.gatherGroup → SelectGroups$Grouped.nextSource
  → SessionLocal.compare`: identical locus to `TestCrashAPI` above
  (row-comparison cost inside grouping). Already on the "candidate" list;
  confirmed.
* **`TestPerfectHash`** — a genuine multi-threaded variant: `main` blocked
  in `Thread.join()` from `MinimalPerfectHash.generateMultiThreaded`, while
  all 16 worker threads are `alive=true blocked=false`, each actively
  executing `MinimalPerfectHash$1.run → generate/hash` at *different* bytecode
  offsets across the dump (pc values 0, 27, 30, 32, 39, 41, 153, 526 — real
  spread, not sixteen threads frozen at the same point). Compute-bound hash
  generation, not row iteration, but the same "genuinely working, just too
  slow for the timeout" shape.

* **`TestStringCache`** — split out of this doc on 2026-08-07 as a suspected
  `Thread.join()` lost wakeup, then **refuted and folded back in** the same
  day (see
  [`bug-h2-teststringcache-thread-join-REFUTED-20260807.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-teststringcache-thread-join-REFUTED-20260807.md)).
  It belongs here, with one twist worth knowing: **the test is not the slow
  part.** `TestStringCache.main` runs `testFromMain()` and *then*
  `new TestStringCache().runBenchmark()`, and the suite runner invokes
  `main`, so the benchmark is unavoidable.

  | arm | test only | full `main()` |
  |---|---|---|
  | HotSpot | 0.38 s | 5.17 s |
  | CratonVM, JIT | 1.78 s | **441.84 s** (≈85×) |
  | CratonVM, `--nojit` | 3.39 s | ≫300 s |

  The test half is 4.7–8.9×, i.e. ordinary. The benchmark half is 85×:
  `runBenchmark → testToUpperCache → StringUtils.toUpperEnglish /
  String.toUpperCase → StringUTF16.toUpperCase → Character.toUpperCaseEx`,
  at varying bci across four captures. A **case-mapping / string-building**
  hot spot rather than this doc's MVMap-and-commit one, but the same family:
  progressing, far too slowly for any per-class timeout. The three
  `Thread.join()`s in `testMultiThreads` return normally — 20/20 clean
  test-only runs under `--nojit`.

  Note for anyone reading a watchdog capture of this class: `main`'s summary
  row shows `deposit=STALE` with a `java/lang/Thread.join@129` chain. That is
  a *deposit* from the join that already returned; the live stack dump above
  it is authoritative. The `deposit=` tag exists because this class is what
  exposed the gap.

Everything examined in this sweep fits this doc's throughput-cliff family.
