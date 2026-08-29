# ZGC arena fragmentation — the occurrences outside H2, and the re-verification they are waiting for

## Status

**OPEN as a VERIFICATION item, 2026-08-29.** Split out of
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`, whose three
repairs (large-object-end compaction, the starved TLAB-refill floor, and the
region tripwire) landed on 2026-08-29 and were measured on the H2 classes only.

This page exists so the occurrences of the same symptom in **other projects**
are not lost, and so nobody reads "the H2 classes pass" as "the family is
closed" without running them.

## The symptom to match on

```text
zgc frag gauge: the arena is broken up — N% of the heap is free but the
largest single block is only M% of capacity
```

…followed by either `OutOfMemoryError` on a request larger than that block, or
a livelock in which the guard's own `occurrence` counter doubles on every firing
(32768 -> 1048576 -> …), which is the exponential-retry shape. Both are the same
defect wearing different clothes: the heap has the bytes and cannot hand out a
contiguous run.

## Occurrence 1 — Spring Framework, unrelated to H2 or Hibernate

`org.springframework.http.client.SimpleClientHttpResponseTests` (an HTTP-client
test full of Mockito mocks) timed out at 300 s during a full-suite ZGC run,
was isolated and rerun alone at a 400 s cap, and still hangs. Its log carries
the gauge line above at **95.8 % free, largest servable block 0.0 %**, and the
occurrence counter doubles on every subsequent firing — the same shape as
`org.h2.test.jdbc.TestCachedQueryResults`'s livelock.

Recorded 2026-08-29 as a third confirmed occurrence, so a future reader knows
this defect is not H2/Hibernate-specific. **No `CRATONVM_DBG_*` census was ever
taken for this class**, so it is not attributed to any specific obligation — it
is an occurrence, not a diagnosis.

Repro: run that class alone with any timeout above ~300 s.

## Occurrence 2 — Hibernate, 2026-08-11, the same wall one size down

`sql.exec.SmokeTests` and
`boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` both died with
`OutOfMemoryError: Java heap space` while the guard reported
`free_list_bytes=1211378512 largest_free_block=65528` — 1.13 GiB free, no hole
big enough for one 65 552-byte `DFAState[8192]`. Both pass on the same heap with
`CRATONVM_ZGC_TLAB=0` and on G1.

That reading is what raised `ZGC_TLAB_MAX_CHUNK` from 64 KiB to 512 KiB, which
moved the ceiling rather than removing it — the reasoning recorded on
`Arena::high_cursor`. It should be re-measured against the 2026-08-29 floor
change, which is aimed at exactly this shape from the other side.

## What the re-verification has to establish

For each occurrence, on one binary with the arms interleaved:

| arm | what it says |
|---|---|
| default | does the class still fail? |
| `CRATONVM_ZGC_HIGH_COMPACTION=0` | was the large-object end the wall? |
| `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` | was the refill floor the wall? |
| both `=0` | the pre-2026-08-29 behaviour, byte for byte |

and, on the default arm, the two engagement lines — `[GC] zgc-high-compaction:
cycles= declined=` and the `span_hist`/`high_*` fields of the failure report.
A class that stops failing while `cycles=0 declined=N` did not stop failing
because of the high-end compactor, and saying so is the point of printing them.

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the parent page: the whole diagnosis, the three repairs, and the H2
  measurements.
- `docs/known-issues/gc/zgc-rewrite-pass-walks-off-a-reference-array-20260815.md`
  — a different ZGC defect on the same collector; do not conflate them.
