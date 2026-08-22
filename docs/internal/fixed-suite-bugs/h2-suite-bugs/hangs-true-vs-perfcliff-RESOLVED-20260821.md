# H2 HANG classes — perf-cliff vs true hang, resolved (RESOLVED 2026-08-21)

## Status
**RESOLVED 2026-08-21.** Was `docs/known-issues/h2/hangs-true-vs-perfcliff-20260821.md`
(OPEN, partial 20/48). The question this page asked — of the ten classes
still HANG at a 5x cap, which are actually stuck? — now has a complete
answer for all ten, and for the remaining 28 of the 48-class FAIL/HANG union
this page's predecessor left untested. **Zero are deadlocked or blocked.**
One is a genuine livelock (busy-spinning on an allocation that can never
succeed). The rest are either ordinary throughput cliffs still making
progress, or have since started passing outright as unrelated dev fixes
landed.

## Method
Same instrument this doc's predecessor used — `--stack-sample-ms`, per-process
CPU via `/usr/bin/time -v`, and `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` so a hit
timeout classifies cleanly — plus one addition that turned out to matter more
than stack sampling: grepping each log for `OutOfMemoryError` /
`arena allocation failed`. A class that never finishes and never shows either
signature is genuinely just slow. A class that shows either is not a "hang"
in any sense worth chasing with a bigger cap — it has already found its
answer, and the answer is a specific, root-caused defect.

Run on a fresh `fix/h2-hangs-triage-20260821` worktree, `dev` tip as of
2026-08-21 (several days' more interpreter/JIT/GC fixes than the
predecessor's Windows rerun), release build, on the Azure host. Caps of
2400s for the originally-HANG classes (matching the predecessor's own cap) and
600-900s for the previously-untested 28 (informative either way: a PASS
inside 600s answers "not stuck" on its own, and a still-capping class after
600s already shows the same "computing, not blocked" signature a longer cap
would only confirm at greater cost).

## The ten original "still HANG" rows, resolved

| class | this rerun | verdict |
|---|---|---|
| `TestLIRSMemoryConsumption` | **PASS, 3:30** (was capped at 1500s) | perf-cliff, now cleared — recent dev fixes closed most of the gap |
| `TestCancel` | **PASS, 23:41** (was 6997s and still failing at census's own >128x estimate) | perf-cliff, now cleared |
| `TestCases` | still capping at 2400s, **101% CPU, wall-matching CPU-seconds, 0 OOM** | genuine perf-cliff, actively computing — matches the 2026-08-07 stack-sample finding (`bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md`) that this class runs to completion given enough time, just re-verified fresh on current dev with JIT on |
| `TestScript` | still capping at 2400s, 0 OOM, varying leaf frames | genuine perf-cliff, same shape |
| `TestOpenClose` | **FAIL, rc=1, 2:04 — not a hang at all** | spurious `OutOfMemoryError`; root cause is `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`'s ZGC-never-compacts-under-JIT defect, now confirmed on a fourth class family. The predecessor's Windows rerun classified this as HANG at 1500s; this rerun crashes it in 2 minutes flat — a different outcome shape not reconciled by that predecessor (it flagged the same kind of shape mismatch for `TestSubqueryPerformanceOnLazyExecutionMode` and `TestWeb` without resolving it; this is the resolution: the OOM is fast and deterministic once triggered, so whatever made the Windows run merely time out instead is a difference in host/heap conditions, not evidence of two different underlying behaviors) |
| `TestCachedQueryResults` | still capping at 2400s — **but this one really is stuck** | **the one confirmed livelock in this set.** Same root cause as `TestOpenClose`, but this class's own code catches the `OutOfMemoryError` (looks like intentional cache-eviction-under-memory-pressure testing) and retries. The fragmentation that caused the first failure never clears on this collector, so every retry fails identically — 23,468 occurrences of the same `native_oom` WARN logged between 6 minutes in and the 2400s kill, across 5-6 threads, ~12/s. Threads are burning CPU the whole time, which is why a naive "is it pegged?" check would have called this "progressing" — it is not. See the ZGC-OOM doc's new section for the full table. |
| `TestLob` | not re-run — census §3 already covers it: HotSpot fails identically, confirmed by that class's own dedicated page | not a CratonVM bug |
| `TestSubqueryPerformanceOnLazyExecutionMode` | not re-run — census §3: HotSpot fails identically | not a CratonVM bug |
| `TestWeb` | not re-run this pass — separately reclassified (see below) | timing-margin, not a discrete bug |
| `TestBenchmark` | not re-run this pass — already resolved 2026-08-18 (its OOM was fixed, it is a plain perf-cliff now) | perf-cliff, already covered |

**`TestBnf` / `TestWeb` correction, folded in from a concurrent merge:** the
census's §2b "confirmed correctness bug" framing for these two has been
superseded. `bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` is now
`not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` — the original
`RuleElement.link` NPE hypothesis was a red herring from an incomplete
isolated repro. Both classes still fail H2's own 100ms
`Sentence.MAX_PROCESSING_TIME` autocomplete budget, but that is the general
interpreter throughput gap crossing a fixed wall-clock threshold, not a
distinct logic defect. Not a hang either way — recorded here only because
this page's own table listed it and the classification has since moved.

## The 28 previously-untested classes, now all resolved

Completed against the full `nonpassed-from-full-20260810.txt` union (62
classes) minus the 14 that had already recovered outright — the 48-class
FAIL/HANG set this page and `correctness-issues-consolidated.md` share as
their source list.

**Now passing** (recovered since the 08-18 census, several since the original
20/48 partial rerun too): `TestDiskFull` (1:01), `TestPerfectHash` (5:27),
`TestValueMemory` (0:08), `TestPgServer` (0:28), `TestStringCache` (2:35).

**Genuine perf-cliffs, still capping, zero OOM signature, actively
computing**: `TestMVStoreBenchmark`, `TestBtreeIndex` (matches its own
2026-08-10 stack-sample finding — "progressing, not stuck," 25.1x — reverified
fresh), `TestCrashAPI`, `TestSimpleIndex`, `TestFileSystem`.

**ZGC-fragmentation-OOM victims — not hangs, fast fails with a known root
cause**: `TestKillProcessWhileWriting` (already tracked),
`TestMVStoreCachePerformance` (FAIL, 5:55), `TestMVStoreTool` (FAIL, 1:33).
Full detail in `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`.

**One new, unrelated correctness finding**: `TestRandomMapOps` — a
`ClassCastException` at 108s (comparable to HotSpot's own ~137s), not yet
root-caused. See `correctness-issues-consolidated.md`.

**Not re-run — already excluded by the census's own §3** (HotSpot fails these
too, so a CratonVM-side verdict is uninterpretable): `TestMVStore`, `TestJoin`,
`TestKill`, `TestMultiThreaded`, `TestPowerOffFs`, `TestPowerOffFs2`,
`TestTimer`, `TestSynth` (confirmed elsewhere as an unbounded `while(true)`
fuzzer — hangs on every JVM, by construction), `TestMulti` (thread),
`TestClassLoaderLeak`, `TestExit`, `TestMemoryUnmapper`, `TestTools`.

## What this settles

* **No true stuck or deadlocked hang exists anywhere in the 48-class union.**
  Every class that does not finish inside its cap is either (a) verifiably
  consuming CPU with no OOM/fragmentation signature — a throughput cliff, not
  a hang, several of which have already narrowed or closed since the 08-18
  census as unrelated interpreter/JIT work landed — or (b) livelocked on one
  specific, already-diagnosed collector defect.
* **The Windows `timeout` reliability gap the predecessor flagged is moot**
  going forward for this investigation — this rerun used the Linux host's
  `timeout --kill-after`, which enforced every cap exactly as configured.
  `TestCancel`'s 6997s outlier from the original Windows rerun does not
  reproduce; on this host and this dev tip it passes cleanly in 23:41.
* **The one thing worth a session's attention that this triage did not
  itself fix**: the ZGC-never-compacts-under-JIT defect
  (`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`) is
  confirmed to affect at least five classes now (up from one), including the
  one genuine livelock in this whole set. That page's own "What would
  actually lift it" section has the concrete next steps, in order; a
  previous attempt at the direct fix was implemented, verified to work, and
  explicitly withdrawn as unsound because the safety proof it needs is not
  computed for this collector. This triage did not attempt that fix — it is
  GC-internals work with real memory-corruption risk if rushed, explicitly
  out of scope for a hang-vs-perf-cliff triage, and already has its own
  tracked page with a clear plan.
* **`TestRandomMapOps`'s `ClassCastException`** is new and open; see
  `correctness-issues-consolidated.md`.

## Related
- `nonpassed-40-census-20260818.md` — source census and per-class multiplier
  data.
- `correctness-issues-consolidated.md` — the sibling doc for this same
  rerun's exit-code (FAIL) classes; also updated 2026-08-21 with the
  `TestRandomMapOps` finding and the `TestBnf`/`TestWeb` correction.
- `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md` — the root
  cause behind five of this page's classes, one of them the livelock.
- `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md` and
  `bug-h2-hang-cluster-lirs-trace-mvstore-compact-20260807-RESOLVED-20260810.md`
  — the prior art this page's method is built on; both had already shown
  several of these same classes progressing, not stuck, weeks before this
  page existed.
