# Two thirds of an uncontended `ReentrantLock` pair has never been attributed

| | |
|---|---|
| **Status** | OPEN — the total is measured, the terms are not, and the two attempts so far both stopped at the first expensive thing they found |
| **Severity** | high — AQS is a VM-wide primitive |
| **Opened** | 2026-08-05, retiring `aqs-thread-handoff-latency` and `native-call-funnel-is-the-per-call-floor` |
| **Inherits** | `TestAsyncMessagesPerformance.testAsyncTiming`, and the `SmokeTests` concurrency ceiling as a suspected relative |

## The arithmetic that closes neither predecessor

An uncontended `ReentrantLock.lock()` + `unlock()` measures **10,502 ns**
against HotSpot's **14.8 ns** (`probes/AqsBreakdownProbe.java`, quiet host,
2026-08-03). Two documents have now explained that number, and both explanations
were arguments about the largest term they had found rather than a sum:

| revision | claim | why it does not close |
|---|---|---|
| 1 | `AbstractQueuedSynchronizer.acquire`'s pre-park spin (up to 255 `onSpinWait` rounds) | the *uncontended* path never spins or parks, and measures the same |
| 2 | 16 nested Java calls at a 431 ns per-call floor | an ordinary Java call converges to 8.4 ns; the 431 was one unconverged warm-up pass |
| 3 | the five NATIVE calls the path makes, at the funnel's 330-810 ns each | see below — that is at most a third |

Revision 3's own census (`probes/LockNativeCensusProbe.java` under
`--dump-native-registry`) is exact and is not in doubt: `Thread.currentThread()`
x2, `AbstractOwnableSynchronizer.setExclusiveOwnerThread(Thread)` x2,
`jdk/internal/misc/Unsafe.compareAndSetInt` x1. Nothing else on the path is
native. Adding up its own per-native figures:

| | |
|---|---:|
| measured pair | **10,502 ns** |
| 5 native calls at 330-810 ns | ~2,500-4,000 ns |
| 16 nested Java calls at 8.4 ns | ~138 ns |
| **unattributed** | **~6,400-7,900 ns** |

**Two thirds of the cost has no owner.** And the 2026-08-04 leaf-native work is
the confirming experiment: it removed ~90% of the funnel cost from two of the
five natives (`Thread.currentThread()` 564 → 53 ns from compiled code, verified
by hit counter), which by the table above should move the pair by single-digit
percent — and nothing in that session's runs contradicted that.

## What to do differently

The two predecessors each found a real, expensive mechanism and then assumed it
was *the* mechanism. Do not open the third attempt that way.

1. **Account for the whole 10,502 ns before proposing a fix.** A candidate that
   explains 3 us is not an answer to a 10.5 us measurement, however real it is.
   The residual is the finding until it is zero.
2. **Do not trust `--dump-native-registry` as a cost census.** It counts
   *registered native dispatches*. Bytecode, interpreter dispatch, monitor
   operations, safepoint polls, GC barriers and the JIT's own bailouts are all
   invisible to it, and any of them could hold the missing two thirds.
3. **Use a time-weighted profiler, not a call-count trace.**
   `--stack-sample-ms N` is the sampling profiler;
   `--stack-dump-on-timeout` is a call-count trace and will point at whatever is
   called most, which is how revision 1 happened.
4. **Check whether the bodies run compiled at all.** `CRATONVM_DBG=jit-compiled`
   reportedly shows `ReentrantLock.lock`, `ReentrantLock$Sync.lock`,
   `NonfairSync.initialTryLock` and `AQS.release` all compiled — but "compiled"
   was checked, not "executed compiled". An OSR-compiled body that the caller
   never enters through its compiled entry is a plausible several-microsecond
   term and nobody has ruled it out.

## Two leads that are cheap and unexcluded

Neither is claimed as the answer. Both are on the path, both are more expensive
than they look, and neither has been measured.

* **`setExclusiveOwnerThread` does a field store BY NAME.** Its body is
  `ctx.set_field_by_name(this, "exclusiveOwnerThread", …)` plus
  `ctx.record_jmx_owned_synchronizer(…)`. A name-keyed field store resolves a
  string against the class's field table on every call, and this runs **twice
  per pair**. (The JMX half of it was Θ(threads) until 2026-08-05; that is
  fixed, and the fix is not this document's subject.)
* **`Unsafe.compareAndSetInt` at 808 ns.** It is capability-classified
  (`RawMemory`), so it can never take a leaf path, and it must reach a real CAS
  — but 808 ns is not a CAS, it is the funnel plus whatever
  `compare_and_swap_field` does, which includes taking the monitor table's
  per-object CAS lock.

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\AqsBreakdownProbe.java probes\LockNativeCensusProbe.java
& "$jdk\bin\java.exe" -cp out AqsBreakdownProbe                       # HotSpot control: 14.8 ns
& <cratonvm.exe> --java-home $jdk -cp out AqsBreakdownProbe
& <cratonvm.exe> --stack-sample-ms 1 --java-home $jdk -cp out AqsBreakdownProbe
```

Read the `empty instance call (the scale)` row first: it is the load
normalizer, and a run whose scale moved between the first and last rung is not
a measurement. This host's scale drifted from 11.2 to 20.7 ns inside one
session, which is enough to invent or hide a 2x.

## What is already ruled out — do not re-run these

From the retired `aqs-thread-handoff-latency`
(`docs/internal/aqs-thread-handoff-latency-RETIRED-20260805.md`), all measured,
all inert:

* OSR starving callees of the hotness signal (`tier-osr-backedge=2000000000`)
* `CRATONVM_JIT=direct-callee-calls`, `ir-direct-call`, `guarded-virtual-inline`
* JIT admission — the lock methods do compile
* **narrowing the `java/util/` virtual tier-up exclusion** — measured 2026-08-05
  and it is a ~30% *regression* for exactly these bodies (0.77x, 0.76x across
  two runs, `probes/JavaUtilTierUpExclusionProbe.java`). Leave it alone.
