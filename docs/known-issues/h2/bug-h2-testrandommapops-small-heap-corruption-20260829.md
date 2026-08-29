# `TestRandomMapOps` at a small heap — heap corruption with THREE faces, and no reproducer worth bisecting yet

## Status

**OPEN, split out 2026-08-29** from
`bug-h2-testrandommapops-classcastexception-20260821.md`, which retired with
its own titular defect closed: the recorded `ClassCastException` never
reproduced in eleven runs across seven configurations, its seed passes on
CratonVM *and* stock HotSpot 25, and the one live failure that page did
root-cause (the G1 `NoSuchMethodError Object.toArray()`, an unpinned receiver
across a GC point) was fixed on 2026-08-24.

This page carries the row that page could not close: **`org.h2.test.store.TestRandomMapOps`
at `--Xmx 256m` corrupts the heap, at roughly one failure in three runs of
~20 minutes, with a DIFFERENT signature every time.**

## The three faces

All on the post-`COLL-REFRESH`-fix binary, default collector (ZGC),
`--Xmx 256m`:

| rep | cap | outcome |
|---|---|---|
| 1 | 1300 s | clean to cap |
| 2 | 1300 s | clean to cap |
| 3 | — | **SIGSEGV at 155 s** |
| (earlier) | 1500 s | clean to cap |
| (earlier) | — | `NoSuchMethodError` at 845 s |
| (earlier, `dev`) | — | `NullPointerException: "d" is null` at 823 s |

```text
NoSuchMethodError: 'boolean <unknown class 2460030832>.equals(java.lang.Object)'
  at org/h2/test/store/TestRandomMapOps.assertEquals(TestRandomMapOps.java)
  at org/h2/test/store/TestRandomMapOps.testOps(TestRandomMapOps.java:162)
```

Two readings the parent page established, both worth having before spending
runs:

* **`2460030832` is not a class id.** It reads as a truncated pointer, so the
  cell had been REUSED, not merely zeroed. That places this at the opposite end
  of the stale-reference family from the G1 defect the parent page fixed, whose
  receiver was an all-zero header (`ClassId(0)` = `java.lang.Object`). The two
  ends need different questions: *who freed it* versus *who else allocated over
  it*.
* **The segfault's Java frames are not a location.** They name
  `TzdbZoneRulesProvider.load` / `BufferedInputStream.fill`, and the crash
  header says in as many words that frames are "published at the last
  blocking/safepoint deposit — may lag the faulting instruction". The registers
  (`rax=0x0000FFFFFFFFFFFC`, `r10=0xFFFFFFFFFFFFFB05`, unreadable) look like a
  length or index computed off a bad header, consistent with the other two faces
  and NOT with a timezone-loading defect.

`CRATONVM_DBG_COLL_REFRESH` reports **zero** engagements on the failing runs, so
the receiver-pinning path the parent page fixed is not involved at all.

## 2026-08-29: it is CHEAP now — 2/2 in under 90 s on a quiet host

The page's own next move was *"make the defect cheaper before diagnosing it"*.
That happened, and not by tuning the heap: on the 2026-08-29 tip, `--Xmx 256m`,
host load ~3:

| rep | rc | secs | `oom` | `arena` | failure |
|---|---:|---:|---:|---:|---|
| 1 | 1 | **88** | 0 | 0 | `NullPointerException` at `TestRandomMapOps.openStore`, `seed:-67298774724213935 op:1349` |
| 2 | 1 | **46** | 0 | 0 | `NullPointerException` |

**Two failures in two runs, in 46 and 88 seconds**, against a documented base
rate of roughly one in three runs of twenty minutes. Zero `OutOfMemoryError`
and zero arena failures, so this is not the fragmentation family — it is the
corruption this page is about, arriving an order of magnitude sooner.

The collector census from the first of them, on a 256 MB heap:

```text
collections=30 compaction_cycles=24 objects_relocated=159832
relocation_skipped_jit=6 relocation_on_proven_jit=24
zgc-high-compaction: cycles=12 declined=12 vacated_spans=331
                     vacated_bytes=691546832
```

**The obvious hypothesis is that the 2026-08-29 vacated-span publication raised
the rate, and it is the one the parent work predicted in writing.** Before that
change, a span the slide emptied under a pinned cursor was leaked — so a holder
still naming a vacated address met a zeroed corpse. It is handed back to the
allocator now, so the same stale read meets whatever was allocated over it,
sooner and louder. `bug-h2-testmultithread-mvstore-writer-object-identity-20260816.md`
carries the same warning for the same reason.

**That hypothesis needs the arm, not the argument.** The A/B is
`CRATONVM_ZGC_PUBLISH_VACATED=0` on the same binary, and until it is run this
section claims only what it measured: 2/2 at under 90 s on the current tip.

Either way the page gains: a defect that reproduces in a minute is one somebody
can bisect, which is exactly what its own "repro-and-dump is the wrong
instrument at this rate" paragraph was waiting for.

## Why "repro and dump" WAS the wrong instrument

One failure in three, twenty minutes a run, and a different symptom each time
means an attempt costs an hour and buys a signature nobody has seen before.
A clean arm shorter than the base rate says nothing — the same trap the parent
page documents for the `ClassCastException` and the reason its recorded seed was
retired.

**The base rate has to come down in cost before a kill-switch bisect means
anything**, because only then does a clean arm carry information.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 256m \
    -c "$CP" org.h2.test.store.TestRandomMapOps
```

`CRATONVM_DBG_CCE_BT=1` dumps the offending receiver's shape, frame stack and
move history at the dispatch miss. `CRATONVM_DBG_COLL_REFRESH=1` counts receiver
moves the native-collection pinning absorbs — it is what proved the G1 defect
fixed and what proves this one is a different site.

`org.h2.test.store.SeededRandomMapOps` (added 2026-08-24) reflects into the
private `testOps(String,int,long)` so a seed can be pinned; it prints
`SEEDED_PASS`/`SEEDED_FAIL` per rep and exits non-zero on a failure, so it is
usable as a bisect target once one exists. It does not reproduce this row —
the failure is a GC/JIT schedule, not a function of the operation sequence.

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testrandommapops-classcastexception-20260821-RETIRED-20260829.md`
  — the parent page, with the G1 fix and the eleven-run non-reproduction.
- `docs/known-issues/gc/G30-1-the-silent-reference-slot-coercion-20260817.md`
  — the WARN family this may or may not belong to. The parent page's own
  history is that reading that WARN as a discriminator was wrong twice.
- `docs/known-issues/h2/correctness-issues-consolidated.md` — indexes this
  alongside the rest of the 48-class union's correctness results.
