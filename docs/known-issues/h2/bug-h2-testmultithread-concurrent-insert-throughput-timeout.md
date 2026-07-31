# `TestMultiThread.testConcurrentInsert` blows H2's own 5-minute per-job budget — JDBC insert+commit is 250–600x slower than HotSpot

## Status
**OPEN** — measured, not root-caused. Split out of
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md`,
which listed `TestMultiThread` as a suspected downstream symptom of the
`PriorityBlockingQueue` corruption. It is not: with that bug fixed the class no
longer hangs, and shows no `RemovedPageInfo` `ClassCastException`, no MVStore
panic and no "The database has been closed". What is left is throughput.

## Severity
**MEDIUM** — one test class fails, but the underlying number (a *single-threaded*
`INSERT` + `commit` loop running 258x slower than HotSpot, with no contention of
any kind) is a general JDBC/MVStore write-path cost, not something specific to
this test.

## Affected test class
`org.h2.test.db.TestMultiThread` (`testConcurrentInsert`) — 25 threads, each
doing 1000 `INSERT` + `commit` pairs on one file-backed database, each future
awaited with `job.get(5, TimeUnit.MINUTES)` (`TestMultiThread.java:327`).

## Symptom
```
Exception in thread "main" java/util/concurrent/TimeoutException
	at org/h2/test/db/TestMultiThread.testConcurrentInsert(TestMultiThread.java:327)
	at java/util/concurrent/FutureTask.get(FutureTask.java:206)
```
The whole class runs in **4.5 s** on HotSpot (JDK 25) on the same host.

## Measurement

`H2InsertScaleProbe` (in `docs/internal/repros/h2-insert-scale-20260731/`) is
`testConcurrentInsert` reduced to its core: N threads, each with its own
connection, doing `rows` INSERT+commit pairs against one file-backed H2 database.
Same host (Azure `20.83.144.174`), same JDK 25, `-Xmx 1g`, interleaved runs,
CratonVM = `dev` @ `4a48f12cb6` (debug build). All rows below completed with
`failed=0`:

| threads × rows | HotSpot slowest thread | CratonVM slowest thread | ratio |
| --- | --- | --- | --- |
| 1 × 1000 | 195 ms | 50,348 ms | **258x** |
| 4 × 1000 | 233 ms | 137,739 ms | **591x** |
| 25 × 200 | 684 ms | 237,791 ms | **348x** |

Two separate things are visible:

1. **A large constant-factor gap on the single-threaded write path** — 258x with
   exactly one thread and no contention. Whatever this is, it is not a
   concurrency problem, and it is the easier of the two to attack: it needs a
   profile of one `INSERT`+`commit` pair.
2. **Worse-than-HotSpot scaling on top of it** — per-row cost on CratonVM goes
   50 ms → 138 ms → 1189 ms across the three rows above; HotSpot's goes
   0.195 ms → 0.233 ms → 3.4 ms. At the test's real shape (25 × 1000) a single
   job therefore cannot finish inside five minutes, which is exactly how the
   test fails.

For the scaling half, the already-documented global-lock candidates apply:
`jit_activation`'s global `Mutex`, and the H2 MVStore write path's own
`synchronized` regions going through CratonVM's monitor implementation.

The host was at load average 20–48 (16 cores, shared) throughout; runs were
interleaved back-to-back, so the ratios are meaningful even though the absolute
numbers are not.

## Repro
```bash
# HotSpot control
java -Xmx1g -cp "<h2>/target/classes:<probe-dir>" \
  H2InsertScaleProbe /abs/path/scale 25 200
# CratonVM
<cratonvm-bin> --java-home /path/to/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<probe-dir>" H2InsertScaleProbe /abs/path/scale 25 200
```
(H2 2.x rejects a relative database path in the URL — pass an absolute one.)
Or the real test:
```bash
<cratonvm-bin> --java-home /path/to/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```
Reproduced on every run attempted (4/4).

## Note — an intermittent second failure at high thread counts
One 25 × 1000 probe run (348 s) had **all 25 threads** fail with
`JdbcSQLNonTransientException: General error: "java.lang.CloneNotSupportedException"`
on `COMMIT`. That is the array-receiver clone-dispatch bug retired the same day
as
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`,
reappearing on a binary that *contains* that fix — so it looks like a residual of
it that only shows up under concurrency. It is intermittent: a 25 × 200 run of
the same probe on the same binary completed with `failed=0`, and so did the real
`TestMultiThread`. Noted on that doc; not the subject of this one.
