# `TestTransaction.testMergeUsing`: `Expected: 100 actual: 50` — MERGE USING processes half the expected rows

## Status
**OPEN, single-sample** — found 2026-08-07 in a full 218-class suite sweep on
a clean host (`origin/dev` merge @ `f9315411a`, load average ~14).

## The failure
```
09:24.08 org.h2.test.db.TestTransaction Expected: 100 actual: 50
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/AssertionError: Expected: 100 actual: 50
	at org/h2/test/db/TestTransaction.main(TestTransaction.java:35)
	at org/h2/test/TestBase.testFromMain(TestBase.java:479)
	at org/h2/test/db/TestTransaction.test(TestTransaction.java:52)
	at org/h2/test/db/TestTransaction.testMergeUsing(TestTransaction.java:446)
	at org/h2/test/TestBase.assertEquals(TestBase.java:506)
```
9.6 seconds in — fast, deterministic-looking, not a timing/throughput issue.

## Why this looks like a real correctness bug
`testMergeUsing` exercises H2's `MERGE ... USING` SQL statement (SQL:2003
upsert). Expecting 100 and getting exactly **50** — precisely half — is a
suspicious ratio: consistent with either (a) a batch/loop that runs at half
the intended row count (an off-by-factor-of-2 in a row-count computation,
or a source relation that MERGE USING reads being pre-filtered to half its
rows), or (b) every other row silently failing to apply (e.g. an UPDATE
branch of the MERGE overwriting rows that should have taken the INSERT
branch, net-cancelling half of them), or (c) a transactional-visibility
issue where half the merged rows are committed in a state the counting
query can't see yet. A pure timing/flakiness explanation is less likely
given the small, round 2:1 ratio.

## Next steps
* Read `TestTransaction.java:446` (`testMergeUsing`) to see the exact
  MERGE USING statement and what "100" counts (source rows? target rows
  after merge? a specific branch's rows?).
* Isolate to a minimal repro: a small `MERGE INTO target USING source ON
  ... WHEN MATCHED ... WHEN NOT MATCHED ...` over a known row count, and
  check whether CratonVM produces the same 50% shortfall.
* Differential-check against real HotSpot + the same H2 jar to confirm
  this is CratonVM-specific and not an H2-level test issue.
* Check whether the MERGE USING execution path shares code with any
  already-documented dispatch/transaction-visibility bug in this sweep
  (e.g. the `TransactionStore`/`VersionedBitSet` path implicated in
  [`bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame.md`](bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame.md)
  — both are H2 MVStore-transaction machinery, though nothing here
  confirms they share a root cause).

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestTransaction
```
