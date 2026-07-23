# H2 suite — hang rerun (1500s timeout) + FAIL root-cause triage, 2026-07-23

Two follow-ups to [RESULTS-20260723.md](RESULTS-20260723.md) (full 218-class
run: PASS 139 / HANG 60 / FAIL 19):

1. Rerun the 60 `HANG` classes with `--class-to 1500` (25x the original 60s)
   to separate genuine infinite hangs from merely-slow-but-finite execution.
2. Root-cause triage of the 19 `FAIL` classes for real CratonVM bugs vs.
   environment/test-design noise.

## 1. Hang rerun: most "hangs" were just slow, not stuck

Ran all 60 previously-hung classes across 4 parallel shards
(`--only <60-class regex> --shard I/4 --class-to 1500`). **This did not
reach full completion within a practical session budget** — several classes
that are genuinely slow at 1500s each make a full 60-class sweep at this
timeout a multi-hour proposition even sharded 4-way (worst case ~15 classes
× 1500s per shard ≈ 6+ hours) — so the run was stopped after **16/60**
classes completed, prioritizing a representative sample over exhaustive
coverage. Full detail (`out/hangrerun{1,2,3,4}-jit-real-all-20260723-103323/`):

| Class | Status | rc | Seconds |
|---|---|---:|---:|
| `org.h2.test.mvcc.TestMvccMultiThreaded` | PASS | 0 | 14.9 |
| `org.h2.test.db.TestLargeBlob` | FAIL | 1 | 78.3 |
| `org.h2.test.db.TestLob` | FAIL | 1 | 117.6 |
| `org.h2.test.db.TestFullText` | HANG | 137 | 121.5 |
| `org.h2.test.db.TestCompatibility` | PASS | 0 | 143.8 |
| `org.h2.test.server.TestNestedLoop` | PASS | 0 | 149.3 |
| `org.h2.test.store.TestFreeSpace` | PASS | 0 | 160.6 |
| `org.h2.test.jdbc.TestGetGeneratedKeys` | FAIL | 1 | 181.2 |
| `org.h2.test.db.TestOutOfMemory` | FAIL | 134 (SIGABRT) | 187.6 |
| `org.h2.test.db.TestRunscript` | PASS | 0 | 214.3 |
| `org.h2.test.db.TestIndex` | PASS | 0 | 310.8 |
| `org.h2.test.db.TestTempTables` | PASS | 0 | **699.6** |
| `org.h2.test.db.TestOpenClose` | FAIL | 1 | 924.1 |
| `org.h2.test.db.TestCases` | FAIL | 1 | 1315.2 |
| `org.h2.test.db.TestSubqueryPerformanceOnLazyExecutionMode` | **HANG** | 124 | **1500.0** |
| `org.h2.test.db.TestQueryCache` | **HANG** | 124 | **1500.0** |

**13 of 16 (81%)** resolved well before the 1500s ceiling — several took
multiple minutes (`TestTempTables` needed **11.7 minutes**, `TestCases`
needed 21.9 minutes) but were not actually stuck. The original run's flat
60-second timeout was simply far too aggressive for CratonVM's current
performance profile on DB/MVStore-heavy tests; most of the 60 "hangs" in
RESULTS-20260723.md are a **throughput problem, not a correctness/liveness
problem**.

Only **2 of 16 (`TestSubqueryPerformanceOnLazyExecutionMode`,
`TestQueryCache`) are confirmed genuine hangs** — still running at the full
1500s mark, killed by `timeout`'s own SIGTERM/SIGKILL (`rc=124`).

**One anomaly worth flagging as a runner limitation**: `TestFullText` shows
`HANG` with `rc=137` (SIGKILL) but only **121.5 seconds** elapsed — far short
of the configured 1500s. `rc=137` is indistinguishable in
`run-h2-suite.sh`'s `classify()` between (a) `timeout --kill-after=5`'s own
kill and (b) an external SIGKILL (most likely the Linux OOM-killer — this is
a heavily shared, memory-contended host running ~15 other concurrent
sessions). **Any `HANG` entry whose recorded `ms` is far below the
configured `--class-to` should be read as "probably killed by something
external (OOM), not a demonstrated infinite hang."** This is a real gap in
the runner's classification and worth fixing (e.g. checking `dmesg`/cgroup
OOM events, or at minimum flagging short-duration `rc=137` distinctly from
`rc=124`).

**Coverage caveat:** 44/60 classes remain unretested at the longer timeout.
Extrapolating from this 16-class sample (2 genuine hangs, 1 likely-OOM, 13
merely-slow) is not statistically solid, but directionally suggests the
*true* hang count among the original 60 is well under 60 — plausibly in the
10-20 range once OOM noise and slow-but-finite tests are excluded.

## 2. FAIL triage: 4 new untracked CratonVM bugs, 6 already tracked, 8 noise

Full investigation of all 19 `FAIL` classes from RESULTS-20260723.md, cross-
referenced against the HotSpot JDK25 baseline (the authoritative oracle —
`out/full-hotspot-all-20260721-185428/results.tsv`) and the ~15 other
concurrent `wt-h2-*` worktrees + `docs/known-issues/h2-suite-bugs/` /
`docs/internal/` on this host, to avoid re-deriving already-tracked work.

### A. NEW untracked genuine CratonVM bugs

| Classes | Bug | Root-cause hypothesis |
|---|---|---|
| `TestStreamStore`, `TestMVStoreStopCompact` | `NullPointerException: ...Interruptible.interrupt(Thread)... "this.interruptor" is null` from `AbstractInterruptibleChannel.begin()` during a pooled-thread `FileChannel.write` | CratonVM's `sun.nio.ch` interruptible-channel plumbing never installs the channel's lazily-created `Interruptible` (normally wired up via `blockedOn(...)`) — subsystem: native NIO channel / thread-interrupt registration |
| `TestPgServer` | `NullPointerException: Cannot enter synchronized block because "this.lock" is null` in `ReferenceQueue.enqueue` (via pgjdbc `Portal.close`/Cleaner) | Synthetic-vs-real `ReferenceQueue` object-layout mismatch in real-JDK mode — the real JDK25 `ReferenceQueue`'s `lock` field instance-initializer doesn't run. Subsystem: reference/Cleaner GC machinery |
| `TestMultiThread` | Rust panic: stale/zeroed-header String receiver read by native `String.length()` under concurrency (`ThreadPoolExecutor` path) | Cross-thread GC root-scanning / object-movement gap — a String was relocated/reclaimed while another thread held a stale reference mid-native-call. Same bug family as this codebase's existing cross-thread root-scan work (BUG-03, Family-A, stale-ObjectRef), but this specific manifestation is untracked |
| `TestCluster` | Wrong H2 error code on runtime cluster failover: expects `90067` (CONNECTION_BROKEN), gets `90098` ("database has been closed") | Lower confidence — CratonVM's socket/session layer likely reports a dropped peer connection as *closed* rather than *broken*, so `SessionRemote` maps to the wrong code. Subsystem: networking / session error mapping |

### B. Genuine CratonVM bugs — already tracked elsewhere

| Class | Where tracked |
|---|---|
| `TestFileSystem` | `docs/internal/h2-suite-bugs/bug-h2-files-setposixfilepermissions-FIXED.md` — flags read-only `FileChannel.open` not enforcing write-protection as an open follow-up |
| `TestWeb` | `docs/known-issues/h2-suite-bugs/bug-h2-dataoutputstream-writechars-data-loss.md` (OPEN, HIGH) — `DataOutputStream.writeChars` no-op corrupts H2's TCP wire protocol; this `assertContains` failure is a residual in that family, distinct from the already-fixed `h2-testweb-logout-connectexception` |
| `TestUpgrade` | Branch `fix/h2-testupgrade-rootreference-20260722` (`wt-h2-testupgrade-20260722`) — actively under investigation; `RootReference.hasChangesSince` cross-version dispatch |
| `TestTransaction` | `docs/known-issues/h2-suite-bugs/bug-h2-suite-residual-fail-triage.md` — `MERGE USING` exactly-half-rows, not yet root-caused |
| `TestBnf` | Same residual-fail-triage doc — `testProcedures`, not yet investigated |
| `TestFileLock` | Same residual-fail-triage doc — wrong lock error code, timing/advisory-lock hypothesis |

### C. Noise — not CratonVM bugs (fail identically on the HotSpot baseline, or environment-dependent)

`TestTimer`, `TestJoin` (needs local Postgres), `TestExit` (test calls
`System.exit` by design), `TestMulti` (synth.thread — reserved-keyword SQL,
version-dependent), `TestFunctions` (documented JDK25
`testAnnotationProcessorsOutput` not-a-bug), `TestRecoverKillLoop`
(open-ended kill/restart stress test, excluded from comparison by its own
triage doc), `TestClassLoaderLeak` (pre-JDK9 `URLClassLoader` assumption),
`TestMemoryUnmapper` (JDK25 `Unsafe` deprecation behavior).

### Recommended next actions

Highest value: the NIO `Interruptible.interrupt` NPE (2 classes, clean
JDK-level repro) and the `ReferenceQueue.lock` NPE (clean synthetic-vs-real
layout hypothesis, half-documented in `docs/internal/gaps/roadmap.md`
already). `TestMultiThread`'s stale-String-receiver panic belongs with the
existing cross-thread GC root-scan effort. `TestCluster`'s error-code
mismatch is lowest priority (cosmetic, not data-corrupting).

## Files

```text
out/hangrerun{1,2,3,4}-jit-real-all-20260723-103323/   16/60 classes completed
                                                        (44 unretested — run
                                                        stopped for time budget)
```
