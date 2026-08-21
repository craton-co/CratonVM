# H2 HANG classes — perf-cliff vs true hang, what the evidence actually shows

## Status
**OPEN, evidence-based triage, 2026-08-21 — partial (rerun in progress).**
Written from the first 20 of 48 classes completed in a local Windows rerun of
the 2026-08-18 Azure census's FAIL/HANG union, at a 1500s per-class cap (5x
the census's original 300s). Will be updated as the remaining 28 complete.

## Why "HANG" alone does not mean "stuck"
The test harness labels a class HANG purely because it exceeded its
timeout — that is a statement about wall-clock, not about whether the
process would ever finish. The 2026-08-18 census already established (§2a)
that most of these 40 all-collector-fail classes are severe interpreter
throughput cliffs that HotSpot clears in single-digit seconds and CratonVM
needs anywhere from ~2x to >128x longer for, not stuck loops — this doc
extends that check against the actual 2026-08-21 binary (which carries
several months' more interpreter/JIT fixes than the census's `dev@64c02b7ac`).

## Direct evidence gathered this rerun

**6 of the 20 completed classes RECOVERED (PASS) at the 5x cap** — proof by
direct demonstration that hitting a timeout does not mean stuck:

| class | result at 1500s cap | census's own prior read |
|---|---|---|
| `TestCluster` | PASS, 144s | (not previously capped) |
| `TestLargeBlob` | PASS, 1086s | (not previously capped) |
| `TestMultiThread` | PASS, 1390s | census: needs ~19x (231.8s vs 12.2s HotSpot) at the 300s cap — now clears at 5x |
| `TestTempTables` | PASS, 1370s | census §2a: HotSpot 4.4s, CratonVM previously capped |
| `TestReorderWrites` | PASS, 63s | (not previously capped) |
| `TestFreeSpace` | PASS, 1354s | census §2a explicitly estimated **>90x** needed (HotSpot 3.3s) — passing at only ~5x here is a materially better result than the census predicted, consistent with unrelated interpreter/JIT fixes landing on `dev` since 2026-08-18 |

`TestFreeSpace` recovering at 5x when the census estimated >90x is the most
notable single data point — either the census's extrapolation was
optimistic, or (more likely, given the volume of interpreter/JIT fixes that
have landed on `dev` in the days since) the VM has gotten meaningfully faster
at whatever `TestFreeSpace` exercises. Not disambiguated here.

**Live CPU check, 2026-08-21 (one representative sample, not per-class):**
the class running at check time (`TestKillProcessWhileWriting`'s successor in
the list) was consuming CPU actively — 6.64 CPU-seconds over a 6-second
wall-clock window, i.e. pegged, not idle or blocked. This is a single
snapshot, not evidence about each of the 10 classes below individually — see
"What is NOT established" below.

## The 10 classes still HANG at the 5x cap

| class | time | notes |
|---|---:|---|
| `TestCases` | 1501s | census: needs >67x (HotSpot 4.5s) |
| `TestLIRSMemoryConsumption` | 1500s | |
| `TestLob` | 1502s | |
| `TestOpenClose` | 1505s | |
| `TestSubqueryPerformanceOnLazyExecutionMode` | 1501s | HotSpot itself FAILs this one per census §3 — the CratonVM HANG may be masking a different, non-timeout outcome entirely |
| `TestCachedQueryResults` | 1501s | |
| `TestCancel` | **6997s** | census: needs >128x (HotSpot 2.3s); also see harness note below |
| `TestScript` | 1500s | |
| `TestWeb` | 1501s | HANG here, but census records it as FAIL (10.1s) at the 300s cap — a different outcome shape between runs, not reconciled |
| `TestBenchmark` | 1501s | census: this class's original OOM was FIXED 2026-08-18 (`NativeContext::reclaim_before_alloc_retry`); it no longer OOMs and now simply runs the whole workload slowly — squarely in the throughput-cliff category, not a fresh hang |

**`TestCancel` at 6997s (23x the 300s baseline, 4.7x this rerun's own 1500s
cap) is the standout.** The outer `timeout --kill-after=5 1500` did not
actually kill it at 1500s on this Windows box — a harness reliability gap
(Windows `timeout` not reliably enforcing the cap), not itself evidence about
CratonVM. It is *also* the class the census estimated needs the largest
multiplier (>128x) of any row, so both explanations (harness miss, and a
severe-even-by-this-list's-standards throughput cliff) are consistent with
what was observed — not distinguished here.

## What is NOT established
**None of the 10 are confirmed as a true stuck/deadlocked hang** — but
neither is that ruled out for all of them. The evidence available:
* One live CPU snapshot during this rerun (not one of the 10 above,
  incidentally) shows CPU-bound behavior consistent with computation, not a
  blocked wait.
* 6 sibling classes from the same FAIL/HANG union recovered outright at the
  same 5x cap, including one (`TestFreeSpace`) the census thought needed far
  more.
* The census's own per-class multiplier estimates (where available) put
  several of the 10 above needing 60-130x — a 5x cap was never expected to
  clear those, so their continued HANG here is not new information, just a
  restated expectation.

What would actually confirm "true hang" vs "perf cliff, needs more time" for
each of the 10: live CPU/thread-state monitoring **during** the run (not
after a timeout kill), and/or a much larger cap (matching or exceeding the
census's own multiplier estimate) for the specific classes it applies to.
Neither was done per-class in this pass — this doc reports what the evidence
supports, not a guess dressed as a finding.

## Next steps
* For the classes with a census multiplier estimate under ~10x that are
  still HANG here, that would be the real anomaly worth chasing first (none
  identified in this partial 20/48 sample — check the remaining 28 for any).
* Fix the Windows `timeout` reliability gap (or switch to a job-object-based
  kill) before trusting any single-run wall-clock number on this host,
  `TestCancel` being the direct demonstration of why.
* `TestSubqueryPerformanceOnLazyExecutionMode` and `TestWeb` both show a
  different outcome SHAPE against the census (FAIL there, HANG here) — worth
  a closer look once the full rerun is in, rather than assumed identical.

## Related
- `nonpassed-40-census-20260818.md` — source census and per-class multiplier
  data.
- `correctness-issues-consolidated.md` — the sibling doc for this same
  rerun's exit-code (FAIL) classes.
