# Direct memory still exhausts under sustained multi-threaded churn (H2 `TestMVStore`)

## Status
**OPEN**, 2026-08-05 — but **both of the code-side leads below have since been
taken**, and the 30-minute `TestMVStore` re-measurement that would close or
re-narrow this page has NOT been run. Read *Leads* with that in mind: what is
left to do here is the measurement, not (necessarily) more code.

Landed since this page was written (`claude/bytebuffer-jdk-contract-e3e6c6`):

* **Lead 2 is done.** `run_cleaner_actions` is now called beside both ordinary
  allocation-triggered `run_finalizers` calls in `maybe_gc`, not only from
  `force_gc_from_native`.
* **A third gap this page did not name is closed, and it is the one the
  symptom actually points at.** The `OutOfMemoryError` quoted below —
  `Direct buffer memory: tried …, used …, max …` — is *this module's* message,
  raised from `try_reserve` inside `dbb_allocate`. It is NOT raised by
  `bits_reserve_memory`, so the reclaim-and-retry the parent fix added there
  never ran for it. It cannot: `java/nio/Bits` is not in
  `force_native_over_real_jdk_bytecode`, so in real-JDK mode the JDK's own
  `Bits` bytecode wins and our native is dead code — our accounting is only
  consulted from the `Unsafe.allocateMemory0` that the real
  `DirectByteBuffer(int)` constructor calls, and that path had no retry at all.
  `dbb_allocate_collecting` now gives it the same bounded reclaim-and-retry.

  This also explains why the failure is specific to *sustained* churn: the two
  budgets are separate counters that drift, because ours also counts every
  other `Unsafe.allocateMemory` caller. Ours saturates first, while the JDK's
  `Bits` still believes there is room — so the JDK's own `System.gc()`-and-retry
  is never even reached.

Lead 1 (the JIT-borrow bail in `run_cleaner_actions`) is untouched and remains
the best next thing to instrument if the re-measurement still shows the cap
being hit.

The residual of the now-retired
`direct-bytebuffers-are-never-reclaimed-20260805` write-up: transient direct
buffers ARE reclaimed now (that page's reproducer matches HotSpot exactly), but
H2's `org.h2.test.store.TestMVStore` still reaches `MaxDirectMemorySize` and
dies.

## Severity
**MEDIUM.** One H2 class, and only after ~30 minutes of it. But the shape — a
background writer thread minting one direct buffer per unit of work — is what
every NIO server does, so the ceiling is probably not H2-specific.

## Symptom

```
org/h2/mvstore/MVStoreException: java.lang.OutOfMemoryError:
  Direct buffer memory: tried 9445376, used 1064486912, max 1073741824
    at org/h2/mvstore/FileStore.lambda$serializeAndStore$0(FileStore.java)
    at java/util/concurrent/ThreadPoolExecutor$Worker.run
    at java/util/concurrent/ThreadPoolExecutor.runWorker
    at java/util/concurrent/FutureTask.run
```

Note the thread: the allocation and the failed reservation both happen on an
**H2 background writer pool thread**, not on `main`.

## Measured, merged `dev` @ `2396b3685` + the reclamation fix, `--Xmx 1g`, JDK 25

| build | outcome |
| --- | --- |
| before the reclamation fix | `Direct buffer memory` early, class dead in ~11 min |
| after | 4 occurrences, class reaches ~965/4500 of a later sub-test at ~30 min, then dies |

So reclamation is demonstrably working (the class runs ~3× longer and gets far
past where it used to die) and is still not keeping up. `probes/DirectBufProbe.java`
— the same allocate-and-drop loop on `main`, single-threaded — passes cleanly at
3200 MiB, so this is specifically the sustained/multi-threaded case.

## Reproducing

```bash
H2=<apps/h2database/h2>
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
cd <fresh dir>          # H2 writes ./data
<cratonvm> --java-home <jdk25> --Xmx 1g -c "$CP" org.h2.test.store.TestMVStore
```

Budget ~30 minutes. `grep -c 'Direct buffer memory'` on the output is the metric.

## Leads, in the order worth trying

1. **`run_cleaner_actions` bails when a JIT borrow is live.** It returns early on
   `crate::jit::helpers::is_jit_thread_set()`, leaving the actions queued for
   "the next top-level (non-JIT) safepoint". On a pool worker running compiled
   code, that condition may hold every time the reclaim-and-retry inside
   `Bits.reserveMemory` forces its collection — in which case the retry collects
   but never runs a single cleaner. `CRATONVM_DBG_CLEANERS=1`-style tracing at
   the entry of `run_cleaner_actions` (printing `jit_thread_set`, the pending
   count, and the drained count) answers this in one run; that instrument is
   what cracked the parent bug.
2. **`run_cleaner_actions` has one call site** — `force_gc_from_native`. See the
   "Remaining asymmetry" section of the retired parent write-up: adding it beside
   the two ordinary-GC `run_finalizers` calls in `maybe_gc` is written and
   test-passed but was not landed for want of a measurement. This is that
   measurement, if lead 1 says the drain is being reached at all.
3. **The buffers may genuinely still be reachable.** H2's `FileStore` keeps
   pending write futures; if a completed future retains its buffer, no amount of
   cleaner machinery will help and the fix belongs elsewhere. Check by dumping
   `Bits.reserved` against the count of live `DirectByteBuffer`s at the moment of
   failure.

## Related

* the retired `direct-bytebuffers-are-never-reclaimed-20260805` write-up — the
  parent, with the full chain description and the two disproven hypotheses;
* the retired `h2-testindex-testmvstore-unmasked-20260802` write-up — why
  `TestMVStore` got far enough to hit this at all. Note that class cannot pass
  on this host on **either** VM: stock HotSpot fails it earlier, at
  `testCacheSize`.
