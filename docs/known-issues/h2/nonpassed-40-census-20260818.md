# H2 — the 40 classes that fail on every collector, split by a HotSpot control

| | |
|---|---|
| **Status** | OPEN census, 2026-08-18. Covers the 40 H2 classes that failed under **all three** collectors. |
| **Run** | The 62-class non-passed union from the last full run (the three 08-10 `gcvariant-*-jit-real-all` arms, 218 classes each), rerun on Azure `20.80.105.49` under Generational / G1 / ZGC, `jit-real`, `--max-heap 1g`, 300 s per-class cap, one shard each. Binary built at dev `64c02b7ac`. |
| **Control** | **Stock HotSpot 25 over the same 40 classes, same host, same heap, same cap.** This is the line that decides what is a CratonVM bug. |

## TL;DR — 22 are ours, 18 are not, and only 2 of the 22 are correctness

| | classes |
|---|---:|
| Rerun of the 62-class union: **recovered** (PASS on all three) | 14 |
| Diverge by collector | 8 |
| **Fail on all three collectors** | **40** |
| — of those, **HotSpot also fails** → not a CratonVM bug | **18** |
| — of those, **HotSpot passes** → CratonVM-side | **22** |
| — — of the 22, wall-clock (HotSpot passes in seconds, we hit the cap) | 20 |
| — — of the 22, **real correctness** (we finish in comparable time, wrong answer) | **2** |

Per-arm totals over the 62: Generational 16 PASS / 29 HANG / 17 FAIL · G1 15 / 29 / 18 · ZGC 20 / 25 / 17. HotSpot over the 40: **22 PASS / 5 HANG / 13 FAIL** in 2269 s, against ~9500 s per CratonVM arm.

## 1. Read this before treating any row as a defect

* **18 of 40 fail on stock HotSpot too.** They are H2-level races, environment (network/ports), or tests that need a machine this one is not. Filing them against the VM is wasted work; §3 lists them so nobody re-triages them.
* **One shard per collector.** A row that differs between collectors is one observation per side, not a collector fact.
* **`descriptor-aware field access DESTROYED the value it was handed` appears in all 40 logs — and in 20 of 20 passing logs.** It is uniform background noise here, not a discriminator, and it already has its own page (G30-1-the-silent-reference-slot-coercion-20260817.md). Do not build a story on it. *(This is the second time this shape has nearly produced a false finding; the Tomcat census records the first.)*
* **Three of the 40 already have pages** and are not re-filed here: `TestLob` (!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md — whose own HotSpot control already found the race fires on HotSpot, confirmed again here), `TestMultiThread` (bug-h2-testmultithread-mvstore-writer-object-identity-20260816.md), and `TestBnf`/`TestWeb` (!bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md).

## 2. CratonVM-side — HotSpot passes, we do not (22 classes)

`cap` = hit the 300 s wall. HotSpot time is the same class on the same host.

| class | Gen | G1 | ZGC | HotSpot (s) | CratonVM ZGC (s) |
|---|---|---|---|---:|---:|
| `org.h2.test.jdbc.TestCancel` | HANG | HANG | HANG | 2.3 | cap |
| `org.h2.test.unit.TestBnf` | FAIL | FAIL | FAIL | 2.7 | 9.0 |
| `org.h2.test.store.TestFreeSpace` | HANG | HANG | HANG | 3.3 | cap |
| `org.h2.test.db.TestTempTables` | HANG | HANG | HANG | 4.4 | cap |
| `org.h2.test.db.TestCases` | HANG | HANG | HANG | 4.5 | cap |
| `org.h2.test.server.TestWeb` | FAIL | FAIL | FAIL | 7.8 | 10.1 |
| `org.h2.test.jdbc.TestCachedQueryResults` | HANG | HANG | HANG | 7.9 | cap |
| `org.h2.test.unit.TestFileSystem` | HANG | HANG | HANG | 7.9 | cap |
| `org.h2.test.store.TestKillProcessWhileWriting` | HANG | HANG | FAIL | 9.5 | 147.7 |
| `org.h2.test.db.TestTransaction` | FAIL | FAIL | FAIL | 9.9 | **10.2** |
| `org.h2.test.db.TestMultiThread` | HANG | HANG | FAIL | 12.2 | 231.8 |
| `org.h2.test.scripts.TestScript` | HANG | HANG | HANG | 14.8 | cap |
| `org.h2.test.db.TestOpenClose` | HANG | HANG | HANG | 21.1 | cap |
| `org.h2.test.db.TestLIRSMemoryConsumption` | HANG | HANG | HANG | 23.2 | cap |
| `org.h2.test.unit.TestPerfectHash` | HANG | HANG | HANG | 24.4 | cap |
| `org.h2.test.store.TestMVStoreBenchmark` | HANG | HANG | HANG | 25.5 | cap |
| `org.h2.test.store.TestMVStoreTool` | HANG | HANG | HANG | 26.5 | cap |
| `org.h2.test.store.TestMVStoreCachePerformance` | HANG | HANG | HANG | 39.0 | cap |
| `org.h2.test.synth.TestCrashAPI` | HANG | HANG | HANG | 69.8 | cap |
| `org.h2.test.synth.TestSimpleIndex` | HANG | HANG | HANG | 86.6 | cap |
| `org.h2.test.store.TestRandomMapOps` | HANG | FAIL | HANG | 136.9 | cap |
| `org.h2.test.store.TestBenchmark` | HANG | HANG | FAIL | 169.5 | **28.9** |

### 2a. Eighteen are wall-clock, and they are demonstrably still running

Every `cap` row is the documented VM-wide per-call cost landing on a 300 s budget, not a stall. The logs show work in flight at the moment the cap fired:

```
TestSimpleIndex     04:59.529 ... INSERT INTO TEST_DI...     (an 8.5 MB log)
TestKill            04:25.923 ... TestKill 6
TestSynth           04:15.332 ... TestSynth 68
TestMultiThreaded   Pass #68
TestPerfectHash     2.524032 bits/key (minimal old) in 41435 ms
TestMVStoreCachePerformance   0 ops/ms; 1 thread(s); cache:
```

The required speed-up is the useful number, and it is not uniform: `TestCancel` needs **>128x** (2.3 s → cap), `TestFreeSpace` >90x, `TestCases` >67x, while `TestRandomMapOps` needs only >2.2x and `TestSimpleIndex` >3.5x. **The bottom of that list is close.** Anything needing under ~5x would clear the cap on a throughput win of the size the per-call workstream is already sizing; the >60x rows will not, and should not be quoted as if one fix covers both ends.

Two rows in this group are *not* at the cap and still lost: `TestKillProcessWhileWriting` (9.5 s → 147.7 s, 15.6x, finishes as FAIL) and `TestMultiThread` (12.2 s → 231.8 s, 19x — see its own page).

> **`TestKillProcessWhileWriting` is NOT a wall-clock row (2026-08-21).** It
> fails with `OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576)`
> on a heap that is **97 % free**: ZGC's stop-the-world slide, its only
> defragmentation, declines on every cycle because a compiled frame is live,
> which in a JIT-warm workload is every cycle. The same class passes under
> `--nojit`, on the generational collector, and on HotSpot. **CLOSED 2026-08-29** — the coverage proof it
> waited on landed on 2026-08-21/26, and three further defects behind it were
> found and fixed on 2026-08-29 (the slide discarding what it emptied, the
> large-object end never being compacted, and the TLAB refill floor spending
> that end's reserve). See
> `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
> for the whole measurement chain. The lesson is the one §2b
> already states, applied to §2a: a row that finishes and fails deserves the
> mechanism to be read before it is filed under the throughput story.

### 2b. Two are real correctness — we finish in comparable time and get it wrong

> **Revised 2026-08-18, after both new rows were worked.** This section said
> **four**. It is **two**: `TestTransaction` was not a correctness defect and
> `TestBenchmark`'s was fixed. Both original readings came from believing an
> assertion message over the mechanism behind it, which is what this section
> exists to guard against — so the corrections are recorded here rather than
> quietly dropped.

These do not fit the throughput story and must not be filed under it:

| class | HotSpot | CratonVM | what |
|---|---:|---:|---|
| `TestBnf` | 2.7 s PASS | 9.0 s FAIL | `Expected: true got: false` — existing autocomplete page |
| `TestWeb` | 7.8 s PASS | 10.1 s FAIL | `does not contain: '...'` — existing autocomplete page |

**`TestBenchmark` — FIXED.** `OutOfMemoryError: Capacity: 10616832` at 28.9 s was
not an exhausted heap. `ByteBuffer.allocate` is shadowed by a native whose
backing array made one allocation attempt and threw, skipping the
force-a-GC-and-retry ladder that both bytecode allocation paths run; the heap was
29 MB used of 1024 MB, and repeating the identical allocation one Java statement
later succeeded. Fixed by `NativeContext::reclaim_before_alloc_retry`. The class
no longer OOMs and now runs the whole workload slowly, so it belongs in §2a
below. Retired to bug-h2-testbenchmark-writebuffer-oom-at-1g-FIXED-20260818.md.

**`TestTransaction` — not a correctness bug.** `Expected: 100 actual: 50` is not
"half the rows": the assertion sums **two threads**, and 50 is `50 + 0`. One
transaction ran all 50 merges; the other timed out on the row lock the first
holds — at **key 1**, the first row, so nothing iterated wrong — and its
`SQLException` is swallowed by the test's own `// Ignore`. The budget is the
harness's `TestAll.lockTimeout = 50` ms; HotSpot's winning batch takes 5–8 ms and
ours takes 66–100 ms. Every H2 DML statement is 50–100x here (MERGE is the
*least* affected of the four measured), so this is §2a, not §2b. Retired to
bug-h2-testtransaction-mergeusing-half-rows-DISPROVEN-20260818.md.

The lesson both rows share, and the reason the "same speed" test in the header of
this section is not sufficient on its own: **class wall-clock parity does not
imply statement-level parity.** `TestTransaction` finishes in 10.2 s against
9.9 s only because most of its sub-tests are dominated by sleeps and lock
timeouts. Inside the one window that decided the assertion we were 10–14x slower.
Before filing a row here again, time the operation the assertion depends on, not
the class.

## 3. Not CratonVM — HotSpot fails these too (18 classes)

| class | Gen | G1 | ZGC | HotSpot |
|---|---|---|---|---|
| `org.h2.test.db.TestFunctions` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.db.TestLob` | HANG | FAIL | HANG | **FAIL** |
| `org.h2.test.db.TestOutOfMemory` | HANG | HANG | FAIL | **FAIL** |
| `org.h2.test.db.TestSubqueryPerformanceOnLazyExecutionMode` | HANG | HANG | HANG | **FAIL** |
| `org.h2.test.poweroff.TestRecoverKillLoop` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.store.TestMVStore` | FAIL | HANG | HANG | **FAIL** |
| `org.h2.test.synth.TestJoin` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.synth.TestKill` | HANG | HANG | HANG | **HANG** |
| `org.h2.test.synth.TestMultiThreaded` | HANG | HANG | HANG | **HANG** |
| `org.h2.test.synth.TestPowerOffFs` | HANG | FAIL | HANG | **HANG** |
| `org.h2.test.synth.TestPowerOffFs2` | HANG | HANG | HANG | **HANG** |
| `org.h2.test.synth.TestTimer` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.synth.sql.TestSynth` | HANG | HANG | HANG | **HANG** |
| `org.h2.test.synth.thread.TestMulti` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.unit.TestClassLoaderLeak` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.unit.TestExit` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.unit.TestMemoryUnmapper` | FAIL | FAIL | FAIL | **FAIL** |
| `org.h2.test.unit.TestTools` | FAIL | FAIL | FAIL | **FAIL** |

The shape is coherent: the `synth.*` / `poweroff.*` families are randomized crash-and-recover stress tests, and `TestJoin` / `TestTools` want sockets and TLS this host does not give them (`Socket.connect`, `SSLHandshakeException`). `TestLob`'s page had already reached this conclusion from its own control on 08-10; this run reconfirms it.

**These 18 are not evidence CratonVM is healthy on them** — a class can fail on HotSpot for one reason and on CratonVM for another. They are evidence only that *this* run cannot tell the two apart, so the VM is not the first place to look.

## 4. Also recorded: 14 classes recovered since 08-10

PASS on all three collectors now, having been non-passing in the 08-10 full run — a week of dev fixed them outright:

`TestFullText` · `TestIndex` · `TestMultiThreadedKernel` · `TestSpaceReuse` · `TestMvccMultiThreaded` · `TestMvccMultiThreaded2` · `TestTransactionStore` · `TestKillRestartMulti` · `TestAutoReconnect` · `TestDateTimeUtils` · `TestFileLockProcess` · `TestPageStoreCoverage` · `TestRecovery` · `TestReopen`

Notably `TestDiskFull` also passes under ZGC now; it has a repro harness at `docs/known-issues/repros/h2-testdiskfull-livelock/` that may be retirable.

## 5. Reproduction

The runner has no `--gc` flag. A patched copy with an `H2_GC_FLAG` passthrough is staged at `/data/h2gc-20260817` on the Azure host (`meta/` symlinked to the real fixture so the class index resolves; the shared worktree is untouched):

```bash
cd /data/h2gc-20260817
export CRATONVM_BIN=<cratonvm>  JDK25=/data/toolchain/jdk-25
export H2_ROOT=/data/cratonvm/apps/h2database/h2
export OUTROOT=$PWD/out-zgc  H2_GC_FLAG='-XX:+UseZGC'
bash run-h2-suite.sh run --category all \
    --only "^org\.h2\.test\.db\.TestTransaction\$" --max-heap 1g --class-to 300 --tag probe

# the control that decides whether it is ours
bash run-h2-suite.sh hotspot --category all --only "<same regex>" --max-heap 1g --class-to 300
```

Class lists on the host: `consistent40.txt` (these 40), `nonpassed-from-full-20260810.txt` (the 62). Results under `out-{generational,g1,zgc,hotspot40}/`.
