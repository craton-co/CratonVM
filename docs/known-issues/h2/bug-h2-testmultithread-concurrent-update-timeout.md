# `TestMultiThread.testConcurrentUpdate` times out — 13 min vs HotSpot's 13 s for the whole class

## Status
**OPEN (2026-08-01).** Split out of
`bug-h2-testmultithread-concurrent-insert-throughput-timeout.md` (now retired to
`docs/internal/fixed-suite-bugs/h2-suite-bugs/` — every claim on that page was a
debug-build measurement artifact and none of them survived re-measurement).

The class `org.h2.test.db.TestMultiThread` still fails, but **not where that page
said**. Execution now gets past `testConcurrentInsert` and dies later, in
`testConcurrentUpdate`. That is a different method and a different workload
(UPDATE, not INSERT), and it has never been measured on its own.

## Severity
**MEDIUM** — one test class. But it is the last thing standing between this class
and a PASS, and the insert half of the class is now demonstrably fine.

## Symptom

Release binary of `dev` @ `c8a3ba181d`, `--Xmx 1g`, HotSpot control on the same
host in the same minute:

```
HotSpot    real 0m13.1s   user 0m18.6s   rc=0
cratonvm   real 13m3.9s   user 6m32.1s

Exception in thread "main" java/util/concurrent/TimeoutException
    at org/h2/test/db/TestMultiThread.main(TestMultiThread.java:57)
    at org/h2/test/TestBase.testFromMain(TestBase.java:479)
    at org/h2/test/db/TestMultiThread.test(TestMultiThread.java:68)
    at org/h2/test/db/TestMultiThread.testConcurrentUpdate(TestMultiThread.java:382)
```

(cratonvm prints frames outermost-first, so `testConcurrentUpdate` is the
innermost frame.) `TestMultiThread.java:382` is the
`job.get(5, TimeUnit.MINUTES)` inside `testConcurrentUpdate` — 25 threads over
`objectCount = 10000`, each doing repeated `UPDATE`s.

## A second, separate defect visible in the same run

```
WARN NoSuchMethodError method="java/lang/Object.next()Ljava/lang/Object;"
     caller="org/h2/test/db/TestMultiThread.testConcurrentInsert()V @pc=197"
```

An `Iterator.next()` resolved against `java/lang/Object` instead of the
interface. `@pc=197` is inside `testConcurrentInsert`'s
`for (Future<Void> job : jobs)` result loop. It is logged at WARN and the run
continues, so a fallback path recovers — but HotSpot never emits it, and this is
the same interface/array-receiver dispatch family as
`../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`.
Worth bisecting independently of the timeout.

## What is already ruled out (measured on the insert half — reuse, do not redo)

From the retired page's investigation, all on a release binary:

* **The `org/h2/` JIT ban is not the lever.** Lifting it
  (`CRATONVM_JIT_ALLOW_PACKAGES=org/h2/`) made the insert path ~9% *worse*.
* **There is no concurrency scaling wall on the insert path.** CPU
  (user) time per row is flat across 1/2/4/8 threads (4.69 → 5.56 ms) while
  HotSpot sits at 0.18-0.23 ms. The cost is a roughly constant ~25-30x, not a
  contention blowup.
* **Not heap pressure** (`--Xmx` 1g/2g/4g/8g: no trend), **not the young-GC
  livelock** (`live` never reaches `threshold`), **not the STW cross-thread
  takeover** (`CRATONVM_XT_PEER_DEADLINE_MS` 1/20/200: no effect), **not the
  JIT-root path** (`--nojit` scales identically).
* **`jit_activation`'s global `Mutex` is already gone** (per-thread tables since
  2026-07-31).

Whether any of that transfers to the UPDATE path is unknown — it is a different
workload with row locking and MVCC versioning that the insert probe never
exercised. **Measure it before assuming.**

## Measurement discipline this host requires

The retired page's headline numbers were wrong twice over, and both traps are
live here:

1. **Never quote a debug-build ratio.** A debug cratonvm is ~5-10x slower than
   release; that alone inflated "258x" out of a real ~25-30x.
2. **Never quote a multi-threaded wall-clock number from this box.** It is 16
   cores shared with 15-40 other sessions. The identical shape measured
   152 776 ms and 22 812 ms twenty minutes apart. Use CPU time
   (`/usr/bin/time -f '%U user %S sys'`), round-robin the arms, take min-of-N,
   and record `uptime` beside every number.

## Reproducing

```bash
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Reproduced on the first attempt; HotSpot runs the whole class in 13 s.

## Related
* `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmultithread-concurrent-insert-throughput-RESOLVED-20260801.md`
  — the retired insert page, with the full measurement record and the profile.
* `docs/known-issues/jit-zero-length-array-20260801.md` — a deterministic
  JIT defect found in the same investigation.
