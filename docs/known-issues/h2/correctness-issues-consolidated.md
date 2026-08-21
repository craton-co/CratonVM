# H2 correctness issues — consolidated reference

## Status
**Index doc, 2026-08-21.** Consolidates every class in H2's non-passed
history that is a genuine CratonVM-side *correctness* defect (finishes in
comparable time to HotSpot, produces a wrong answer) as distinct from a
timeout/throughput issue. Compiled while cross-checking a fresh 2026-08-20/21
local Windows rerun of the 48-class FAIL/HANG union
(`nonpassed-40-census-20260818.md`'s source list) against the existing
census's own accounting.

## The 2 confirmed correctness bugs

| class | evidence | doc |
|---|---|---|
| `org.h2.test.unit.TestBnf` (`testProcedures`) | HotSpot 2.7s PASS, CratonVM 9.0s FAIL — `Expected: true got: false`, autocomplete misses a real completion within H2's 100ms `Sentence.MAX_PROCESSING_TIME` budget | `bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` |
| `org.h2.test.server.TestWeb` (`testWebApp`) | HotSpot 7.8s PASS, CratonVM 10.1s FAIL — same autocomplete budget/mechanism as `TestBnf`, same doc | `bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` |

Both share one root cause and one doc — see `nonpassed-40-census-20260818.md`
§2b for how these were isolated from throughput-shaped false positives (two
other rows, `TestTransaction` and `TestBenchmark`, were originally filed here
too and are now retired: `TestBenchmark`'s was a real bug, fixed;
`TestTransaction`'s was disproven — see below).

## Checked against the 2026-08-20/21 rerun — nothing new to add

The 48-class fail/hang union was rerun locally (Windows, `dev` tip, fresh
build) at a 1500s per-class cap specifically to surface anything the
2026-08-18 Azure census's 300s cap might have hidden. Of the first 20 classes
completed, 4 came back FAIL with an exit code (not a timeout):

| class | this rerun | already explained by | verdict |
|---|---|---|---|
| `org.h2.test.db.TestFunctions` | FAIL, 158s | census §3 — HotSpot fails identically | not a CratonVM bug |
| `org.h2.test.db.TestOutOfMemory` | FAIL, 803s | census §3 — HotSpot fails identically | not a CratonVM bug |
| `org.h2.test.db.TestTransaction` | FAIL, 16s | census §2b — disproven 2026-08-18: not "half the rows," a swallowed lock-timeout exception on a harness-internal 50ms budget; retired to `bug-h2-testtransaction-mergeusing-half-rows-DISPROVEN-20260818.md` | not a correctness bug |
| `org.h2.test.poweroff.TestRecoverKillLoop` | FAIL, 2s | census §3 — HotSpot fails identically | not a CratonVM bug |

All four were already accounted for before this rerun started. None is a new
finding. This section exists so the next person who reruns this list and
sees FAIL on these four classes doesn't re-open them from scratch.

**This doc will be updated if the remaining 28 classes in the rerun (still in
progress as of 2026-08-21) surface a FAIL with a signature not already
covered above or by the census's §3 not-a-bug table.**

## Related
- `nonpassed-40-census-20260818.md` — the source census, §2b (correctness)
  and §3 (not-CratonVM, HotSpot fails too).
- `hangs-true-vs-perfcliff-20260821.md` — the sibling doc for this same
  rerun's timeout-classified (HANG) classes.
