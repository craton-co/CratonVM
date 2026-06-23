# H2 — MVStore insert/commit loop performance cliff (timeout "hang")

## Status
**OPEN** — performance, not a deadlock. Tracked as a hang because it exceeds any
practical per-class timeout.

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
