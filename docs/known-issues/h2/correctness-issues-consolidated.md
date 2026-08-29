# H2 correctness issues — consolidated reference

## Status
**Index doc, 2026-08-21, updated 2026-08-21 (rerun completed).** Consolidates
every class in H2's non-passed history that is a genuine CratonVM-side
*correctness* defect (finishes in comparable time to HotSpot, produces a wrong
answer) as distinct from a timeout/throughput issue. Compiled while
cross-checking a fresh 2026-08-20/21 local Windows rerun, then completed
2026-08-21 against a fresh Azure rerun of the remaining classes
(`nonpassed-40-census-20260818.md`'s source list).

## Revision: `TestBnf` / `TestWeb` are not a discrete defect

The original version of this doc listed `TestBnf` and `TestWeb` under "The 2
confirmed correctness bugs," citing
`bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`. That page has since
been retitled `not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` — the
original `RuleElement.link` NPE hypothesis was a red herring from an
incomplete isolated repro (confirmed with a faithful probe that calls
`linkStatements()` the way every real caller does). Both classes still fail —
against H2's own 100ms `Sentence.MAX_PROCESSING_TIME` autocomplete budget —
but that is the general interpreter throughput gap crossing a fixed
wall-clock threshold, not a distinct logic bug with its own root cause.
**There are zero confirmed "wrong answer in comparable time" correctness
bugs in this set as of 2026-08-21.**

## One new finding from the completed rerun: `TestRandomMapOps`

| class | this rerun | detail |
|---|---|---|
| `org.h2.test.store.TestRandomMapOps` | FAIL, 108s (HotSpot needs only ~137s per the census, i.e. comparable time) | `ClassCastException: class java.lang.String cannot be cast to class java.util.Map$Entry` at `TestRandomMapOps.assertEquals`, seed `-418228611310259706` op `1213` |

This is new and **not yet root-caused**. Circumstantial evidence, not proof:
one GC guard WARN fired ~15s before the crash — "a descriptor-aware field
access DESTROYED the value it was handed" (the G30-1 family,
`G30-1-the-silent-reference-slot-coercion-20260817.md`) — followed by an
ERROR-level "in_published_snapshot" line naming `TestRandomMapOps.assertEquals`
as the top frame at that moment. The census's own methodology warning
applies here directly: this WARN shape "appears in all 40 [failing] logs — and
in 20 of 20 passing logs. It is uniform background noise here, not a
discriminator" — so its presence alone proves nothing. What's different this
time is that an actual wrong-typed value followed within seconds on the same
class, which is worth a dedicated investigation rather than either dismissing
the WARN as noise by reflex or assuming it caused the CCE without checking.
**Needs its own root-cause pass**, ideally starting from `CRATONVM_DBG_COERCION=1`
and `CRATONVM_DBG_LAYOUT=1` on the same seed to get the class name behind
`class_id=664`.

## Checked against the 2026-08-20/21 rerun — the first 20 classes, nothing new

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

## The remaining 28 classes — completed 2026-08-21 (Azure rerun)

Continued on the Azure host (`dev` tip, fresh release build,
`fix/h2-hangs-triage-20260821`) rather than the original Windows box. Of the
28, four came back with a non-timeout exit code:

| class | result | verdict |
|---|---|---|
| `org.h2.test.store.TestRandomMapOps` | FAIL, 108s, `ClassCastException` | **new correctness finding — see above, not yet root-caused** |
| `org.h2.test.db.TestOpenClose` | FAIL, 2:04, `OutOfMemoryError` | **superseded 2026-08-29** — the OOM is gone with the ZGC fragmentation repairs; what remains is `Exception in thread "main" java/lang/Object` with no captured frames, split out to `bug-h2-testopenclose-throwable-is-java-lang-object-20260829.md` |
| `org.h2.test.store.TestMVStoreCachePerformance` | FAIL, 5:55, `OutOfMemoryError` | **superseded 2026-08-29** — no OOM and no arena failure at all now; what remains is a WRONG RECEIVER (`NoSuchMethodError` for `Page.isPersistent()` against a `Page$PageReference`), split out to `bug-h2-testmvstorecacheperformance-pagereference-receiver-20260829.md` |
| `org.h2.test.store.TestMVStoreTool` | FAIL, 1:33, `OutOfMemoryError` | the ZGC fragmentation defect, four repairs on 2026-08-29 — see `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` |

The rest of the 28 either passed outright (several recovering fully since
the original rerun — `TestLIRSMemoryConsumption`, `TestDiskFull`,
`TestPerfectHash`, `TestValueMemory`, `TestPgServer`, `TestStringCache`), hit
a wall-clock cap while still visibly computing (throughput, not correctness —
see `hangs-true-vs-perfcliff-RESOLVED-20260821.md`), or are
already excluded by the census's own §3 (HotSpot fails them too). None
surfaced a new correctness signature beyond `TestRandomMapOps` above.

**This rerun is now complete.** All 48 classes in the FAIL/HANG union have a
result.

## Related
- `nonpassed-40-census-20260818.md` — the source census, §2b (correctness)
  and §3 (not-CratonVM, HotSpot fails too).
- `hangs-true-vs-perfcliff-RESOLVED-20260821.md` — the resolved triage: of
  the ten classes that still capped, zero are deadlocked or blocked.
- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the root cause behind three of this doc's four "new" FAILs, retired
  2026-08-29. Two of those three turned out to have a SECOND failure underneath
  the OOM, and each has its own page now.
