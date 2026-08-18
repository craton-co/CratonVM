# `TestTransaction.testMergeUsing` — the MERGE was never wrong — RECLASSIFIED

| | |
|---|---|
| **Status** | **Closed 2026-08-18 as misdiagnosed**, filed the same day. The failure is real and deterministic; the *defect it was filed as* does not exist. |
| **Was filed as** | A **correctness** bug — "a MERGE USING updates half the rows it should". |
| **Actually** | A **throughput** failure. The MERGE touches exactly the rows it should. One of the two transactions never gets to run its batch at all: it times out on a row lock the other holds, because the harness's lock budget is 50 ms and our 50-statement batch takes 66–100 ms. |
| **Belongs with** | `!nonpassed-40-census-20260818.md` §2a (wall-clock), not §2b (correctness). |

## Why the original reading was wrong

The page reasoned from the assertion:

> **"Exactly half" is the lead.** A row count that is off by a clean factor of
> two … points at the merge's source-row iteration or its matched/not-matched
> partitioning terminating early or visiting alternate rows.

It does not. `testMergeUsing` is a **two-thread** test, and the assertion sums
two independent batches:

```java
final int count = 50;
Thread t = new Thread() { public void run() {
    int sum = 0;
    try { ... int[] a = prep.executeBatch(); for (int i : a) sum += i; conn1.commit(); }
    catch (SQLException e) { /* Ignore */ }      // <-- swallows the whole story
    r[0] = sum;
} };
t.start();
... int[] a = prep.executeBatch(); for (int i : a) sum += i; conn2.commit();
t.join();
assertEquals(count * 2, sum + r[0]);             // 100 = 50 + 50
```

`50` is not "half the rows". It is **50 + 0**: one thread completed all 50
merges, the other threw and contributed nothing. The `catch (SQLException e)`
with `// Ignore` is what made this look like a row-count defect — it discards
the only evidence. Printing that exception in an overlay copy of the class ends
the investigation immediately:

```
PROBE bg-thread SQLException: org.h2.jdbc.JdbcBatchUpdateException:
  Timeout trying to lock table "TEST" ... [50200-249] state=HYT00 code=50200
Caused by: org.h2.mvstore.MVStoreException: Map entry <table.3> with key <1> ...
  is locked by tx 2 and can not be updated by tx 1 within allocated time
  interval 50 ms. [2.4.249/101]
```

It fails on **key 1** — the first row of the batch, not the fiftieth. Nothing
iterated wrong.

## The 50 ms is the test harness's own budget

`TestAll.lockTimeout = 50`, applied by `TestDb` as `;LOCK_TIMEOUT=50` on every
connection URL. Both transactions merge the same 50 keys; whichever grabs row 1
first holds it **until it commits**, i.e. for its entire batch. The loser waits,
and it has 50 ms.

| | HotSpot 25 | CratonVM |
|---|---:|---:|
| winner's 50-statement batch | 5–8 ms | **66–100 ms** |
| loser's budget | 50 ms | 50 ms |
| result | both commit, 50 + 50 | winner 50, loser times out at key 1 → 0 |

HotSpot fits inside the budget with 6x to spare. We overrun it by 2x. That is
the whole failure.

## What was ruled out, and how

* **The JIT.** `--nojit` fails identically. Not codegen, not dispatch — the
  page's own suggested first step, answered.
* **`Object.wait`/`notifyAll`.** H2's `Transaction.waitForThisToEnd` is a
  `wait(remaining)` loop woken by `notifyAllWaitingTransactions`. A standalone
  probe measured both halves against HotSpot: `wait(50)` returns after 50 ms on
  both, and notify-to-wake is 0–1 ms on both. The wait mechanism is healthy; the
  timeout is legitimate.
* **The collector.** All three fail identically, and the batch takes the same
  time with the second thread removed entirely (75 ms solo vs 75 ms contended),
  so this is not lock contention or a GC pause — it is the cost of executing the
  statements.
* **Anything MERGE-specific.** It is not. Timing each statement kind against the
  same table, batches of 50, warm:

  | | HotSpot | CratonVM | ratio |
  |---|---:|---:|---:|
  | `MERGE INTO ... USING` | 1.1 ms | 56 ms | 51x |
  | `UPDATE` | 0.42 ms | 39 ms | 93x |
  | `DELETE` | 0.33 ms | 33 ms | 100x |
  | `INSERT` | 0.26 ms | 21 ms | 81x |

  Every H2 DML statement is 50–100x. MERGE is the *least* affected of the four;
  it is simply the one whose test has a 50 ms deadline in it.

## Where the time goes — and why there is no targeted fix here

A `perf record` over a clean 6,000-statement repro is flat: 2,281 distinct
symbols, the largest 4.9% (`execute_frame_from_index`, the interpreter loop),
and no cluster above ~11% when bucketed (JIT helpers 11.0%, interpreter core
10.0%, GC address checks 9.8%, invoke/dispatch 8.9%, native-call funnel 8.6%,
string hash/compare 7.1%, 35% in a long tail). That is the signature of general
interpreter/JIT-helper overhead, not of one defect.

The JIT is engaged but buys only ~1.4x here (56 ms with, 82 ms without), and
`CRATONVM_JIT=-native-shadow-caller-seal` — which unseals the 118 H2 methods the
native-shadow scan refuses to compile, including `Select.queryWithoutCache`,
`IndexCursor.next` and `Parser.prepareCommand` — moves it 0–10%, matching the
measurement already recorded next to that predicate. So the seal is not the
lever either.

**Closing this test needs H2 DML statement throughput, and that is the
already-tracked residual** (the open JIT rows on Rust-helper share and the
native funnel; see `INDEX_jit_notes`). It is not reachable by a change scoped to
MERGE, to transactions, or to the collector, which is what this page was filed
to look for.

## What to change in the census

`TestTransaction` moves from §2b ("four are real correctness — we finish in
comparable time and get it wrong") to §2a (wall-clock). The §2b entry rested on
"same speed, wrong answer", and that is true only at the *class* level: the
class finishes in 10.2 s against HotSpot's 9.9 s because most of its sub-tests
are dominated by sleeps and lock timeouts. At the *statement* level, inside the
one window that matters, we are 10–14x slower.

## Reproduction

Deterministic, ~10 s.

```bash
cd /data/h2fix-wd
CP="$(cat /data/h2fix-cp.txt)"
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1g -XX:+UseZGC \
    -c "$CP" org.h2.test.db.TestTransaction
```

The instruments that produced the numbers above, all Java-side overlays placed
first on the classpath (no VM rebuild):

* an overlay `TestTransaction` that prints the swallowed `SQLException`, the
  two per-thread sums, and a `System.nanoTime` timeline of both batches — this
  is what turns "actual: 50" into "one thread contributed 0, at key 1, at
  +163 ms";
* `SqlKinds.java`, the four-statement-kind comparison above;
* `WaitNotify.java`, the `wait`/`notify` fidelity check.

**Do not** try to sample the running VM with an in-process
`Thread.getStackTrace()` profiler to find the hot Java methods: on this VM that
call returns an **empty array** for any thread that is actually running Java
code (it only returns frames when the target is parked). A sampler built on it
silently drops every in-window sample and attributes the profile to whatever the
thread was blocked in — it produced a confident, entirely wrong answer here
before the standalone `StackProbe.java` check caught it. That is a separate
defect and is filed on its own.
