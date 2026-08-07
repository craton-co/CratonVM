# `TestTransaction.testMergeUsing` — `Expected: 100 actual: 50` is a swallowed 50 ms lock timeout, not a MERGE USING defect

## Status
**RESOLVED 2026-08-07 — the defect this page reported does not exist.**
`MERGE ... USING` applies every row it is asked to, on both of its branches, and
the whole `TestTransaction` class passes on CratonVM the moment H2's lock budget
is large enough to hold one of the two transactions. What is left is the
throughput constant factor, which is not a defect with a fix and already has a
page: `docs/known-issues/h2/h2-update-path-throughput-20260802.md`.

This is the same shape as the retired
`bug-h2-testmultithread-concurrent-update-timeout` write-up — "H2's own
`LOCK_TIMEOUT` timeouts were the slowness surfacing rather than a bug" — with
one difference worth carrying forward: that class's budget is a **10 second**
`LOCK_TIMEOUT`, this one's is **50 milliseconds**, so `testMergeUsing` is the
sharpest instance of the same wall in the suite and fails deterministically
rather than flapping.

## What the original report claimed, and what is true

The report read the exact 2:1 ratio as a correctness defect and offered three
hypotheses: (a) a row-count computed at half its intended value, (b) an UPDATE
branch net-cancelling rows that should have INSERTed, (c) a transactional
visibility gap hiding half the merged rows from the counting query.

**All three are refuted.** `testMergeUsing` runs two connections through the
same 50-statement MERGE batch concurrently, and `TestAll.lockTimeout` — which
`TestDb.getURL` appends to the JDBC URL as `LOCK_TIMEOUT` — defaults to **50**
(`TestAll.java:358`). Whichever transaction loses the race waits on the winner's
row lock, blows the 50 ms wall, and H2 throws out of `executeBatch`:

```
org.h2.jdbc.JdbcBatchUpdateException: Timeout trying to lock table "TEST"
	at org.h2.command.dml.MergeUsing.update(MergeUsing.java:104)
```

The test's own handler eats it:

```java
} catch (SQLException e) {
    // Ignore                       // TestTransaction.java:425
}
```

so that thread contributes `0` to `sum + r[0]` and the assertion sees exactly
half. **The 50 is a lock-timeout casualty, not a lost row.** Nothing about the
number 50 is a "suspicious ratio" — it is one thread's whole contribution, and
it would be exactly half for any `count`.

## The evidence

Everything below is `origin/dev` @ `1082eb446`, release build, real-JDK
(`--java-home /home/victor/jdk25`), `--Xmx 1g`, on the 16-core Azure host with
`uptime` recorded per campaign. The probe is
`apps/h2database-suite-runner/probes/MergeLockBudgetProbe.java`.

### 1. MERGE USING is correct — checked on the row state, not the update count

`MergeLockBudgetProbe verify 50` drives both branches uncontended (pass 1: every
row matches → 50 UPDATEs; pass 2: nothing matches → 50 INSERTs) and then checks
the actual table: row totals, which ids exist, and every `VALUE` flag.
**9 of 9 checks pass on CratonVM, with and without the JIT, byte-identical to
HotSpot.** No row is lost, no row is double-applied, and the INSERT branch
writes exactly `10000+i` for every `i`. Hypotheses (a) and (b) are dead.

### 2. The verdict follows the budget, with the VM held fixed

`MergeLockBudgetProbe contend 50 4 <ms>` — the `testMergeUsing` shape, 4
iterations per budget, one binary, load 8:

| `LOCK_TIMEOUT` | CratonVM | HotSpot jdk-25 |
| --- | --- | --- |
| 1 ms | — | **0 of 4** |
| 3 ms | — | **2 of 4** |
| 5 ms | — | 3 of 4 |
| 10 ms | — | 4 of 4 |
| **50 ms** (what the test uses) | **0 of 4** | 4 of 4 |
| 100 ms | 4 of 4 | — |
| 150 / 200 / 400 ms | 4 of 4 | — |

Same code, same binary, same data; only the wait budget moves, and the verdict
moves with it. Hypothesis (c) is dead too — the rows are visible, the losing
transaction never got to write them.

### 3. The negative control: HotSpot fails identically on a scaled-down budget

HotSpot is ~10x faster here, so give it ~1/10th the budget. Side by side,
CratonVM at the test's own 50 ms and HotSpot at 3 ms:

```
$ cratonvm ... MergeLockBudgetProbe contend 50 2 50
LOCK_TIMEOUT=50ms, 50 merges per thread
  [thread] org.h2.jdbc.JdbcBatchUpdateException: Timeout trying to lock table "TEST"; SQL statement:
  iter 0: main=50 (177ms) thread=0 (300ms) total=50 expected=100  FAIL
  [thread] org.h2.jdbc.JdbcBatchUpdateException: Timeout trying to lock table "TEST"; SQL statement:
  iter 1: main=50 (143ms) thread=0 (227ms) total=50 expected=100  FAIL

$ java ... MergeLockBudgetProbe contend 50 2 3
LOCK_TIMEOUT=3ms, 50 merges per thread
  [thread] org.h2.jdbc.JdbcBatchUpdateException: Timeout trying to lock table "TEST"; SQL statement:
  iter 0: main=50 (20ms) thread=0 (36ms) total=50 expected=100  FAIL
  [main]   org.h2.jdbc.JdbcBatchUpdateException: Timeout trying to lock table "TEST"; SQL statement:
  iter 1: main=0 (15ms) thread=50 (9ms) total=50 expected=100  FAIL
```

Same exception, same class, same `total=50`. Note HotSpot's `iter 1`, where the
**main** thread is the one that loses and the spawned thread banks the 50: which
side survives is a race, not a branch of the MERGE that systematically drops
rows. A "MERGE USING correctness bug" that reproduces on stock HotSpot as soon
as you tighten a timeout is a timeout, not a correctness bug.

### 4. The whole class is green — the budget is the only thing in its way

`RunTx` constructs `TestAll`, sets `lockTimeout`, and runs the real
`TestTransaction` through `init(conf)` / `testFromMain()`:

| `TestAll.lockTimeout` | CratonVM result |
| --- | --- |
| 50 (the default) | **10 of 10 FAIL** |
| 500 | **10 of 10 PASS**, whole class |
| 2000 | PASS |

Nothing else in `TestTransaction` is broken. Note that at 50 ms the wall trips
in more than one place: 9 of the 10 failures are `Expected: 100 actual: 50` from
`testMergeUsing`, and 1 is a bare `JdbcBatchUpdateException: Timeout trying to
lock table` escaping an *earlier* subtest whose main-thread side has no
swallowing handler. Same wall, different victim — exactly what the retired
`TestMultiThread` page saw at its own 10 s budget.

### 5. MERGE is not a slow path — it is the ordinary constant factor

If MERGE USING had its own pathology, the MERGE ratio would stand out from its
neighbours. It does not. `MergeLockBudgetProbe bench 500 3`, ABBA-interleaved
arms (A B B A A B), median of 3, load 5-18:

| 500 ops | HotSpot `-Xint` | CratonVM `--nojit` | ratio |
| --- | --- | --- | --- |
| MERGE | 64.6 ms | 693.5 ms | **10.7x** |
| UPDATE | 50.2 ms | 495.6 ms | **9.9x** |
| SELECT | 32.8 ms | 325.1 ms | **9.9x** |
| INSERT | 24.6 ms | 254.2 ms | **10.3x** |

Flat to within the host's noise. Interpreter against interpreter, CratonVM
costs ~10x on **every** H2 SQL operation, and MERGE is simply the most expensive
statement of the four on both VMs. Handed to the throughput page.

## What this cost, and what would actually close it

Inside the real class, the winning thread's 50-statement batch takes **61 ms**
(`--nojit`) / **70 ms** (JIT) against a 50 ms budget — instrumented copy of
`TestTransaction`, idle host. HotSpot does it in 2-6 ms. So on an *idle* host
CratonVM needs roughly **2-3x** on the H2 statement path to clear the wall, and
the original sighting was at load ~14, where it needs more.

That is not a fix, it is the constant factor, and the throughput page already
records that no symbol on its profile is worth more than ~2x on its own. Do not
reopen this as a MERGE bug.

## Ruled out — do not redo

* **A MERGE-specific defect.** §1 and §5.
* **A JIT miscompile or an `org/h2/` JIT ban.** The failure is identical with
  `--nojit` and with the JIT (10/10 either way), and the throughput page already
  withdrew the ban claim as a null A/B. On this shape CratonVM's JIT is worth
  ~1.3-1.7x, not the ~10x HotSpot's is (647 / 435 ms JIT against 757 / 706 ms
  `--nojit`, ABBA, n=2 each) — nowhere near enough to matter, and irrelevant to
  the real test anyway, which does 50 merges and never reaches the 500-invocation
  warmup threshold.
* **A shared root cause with the `TestTempTables`
  `CloneNotSupportedException`** (the original report's next-step 4). Ruled out:
  that one is a real dispatch defect in the `classid0` family, this one is a
  lock wait that expires. Both touch H2's MVStore transaction machinery, which
  is where the resemblance stops.

## Reproducing

```bash
H2=<h2 checkout>/h2
javac -cp "$H2/target/classes" -d probe \
    apps/h2database-suite-runner/probes/MergeLockBudgetProbe.java

# is MERGE USING correct?            -> 9 of 9 checks pass
<cratonvm> --java-home $JDK25 --Xmx 1g -c "probe:$H2/target/classes" \
    MergeLockBudgetProbe verify 50

# does the verdict follow the budget? -> 0 of 4 at 50 ms, 4 of 4 at 100 ms
for lt in 50 100 200; do
  <cratonvm> --java-home $JDK25 --Xmx 1g -c "probe:$H2/target/classes" \
      MergeLockBudgetProbe contend 50 4 $lt
done

# the negative control: HotSpot fails the same way on a scaled-down budget
$JDK25/bin/java -cp "probe:$H2/target/classes" MergeLockBudgetProbe contend 50 4 3
```

The original failing class, unmodified:

```bash
cd "$H2"
<cratonvm> --java-home $JDK25 --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestTransaction
```

## Related

* `docs/known-issues/h2/h2-update-path-throughput-20260802.md` — where this
  page's residual went, and the page that owns the constant factor.
* the retired `bug-h2-testmultithread-concurrent-update-timeout-RESOLVED-20260802`
  write-up — the same mechanism at a 10 s budget instead of 50 ms.
* the retired `bug-h2-testannotationprocessorsoutput-jdk25-implicit-proc-disabled-NOT-A-BUG`
  write-up — the convention this page follows: confirm against real HotSpot
  before closing a report, and say what the check showed.
