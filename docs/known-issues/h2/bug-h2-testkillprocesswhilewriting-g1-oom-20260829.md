# `-XX:+UseG1GC` fails `TestKillProcessWhileWriting` — a G1 `OutOfMemoryError` with no arena failure

## Status

**OPEN, split out 2026-08-29** from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`, whose ZGC
defect is closed and which never owned this row. The class **passes under the
default collector**; only the explicit `-XX:+UseG1GC` arm fails.

## Why it is a different defect, measured rather than assumed

The parent page's whole subject is `zgc: arena allocation failed` —
fragmentation of the ZGC arena, reported by that collector's own guard. **This
arm produces no `arena allocation failed` line in either era**, before or after
that work, because G1 does not use the arena at all.

The numbers, from the parent page:

| era | outcome |
|---|---|
| when the parent page first measured it | 2 `OutOfMemoryError`, 31–43 s |
| on the fixed binary | 13 `OutOfMemoryError`, 1500 s cap |

**The face varies between runs**, so reproduce it several times before believing
any single one — a one-run reading here has already misled once (the parent page
had to run a five-arm interleaved A/B, `dev` binary against fixed binary, to
establish that the 2026-08-24 relocation work was not responsible; every column
came back identical).

## What to check first

G1 is an evacuating collector, so an `OutOfMemoryError` there is a
*to-space exhaustion or humongous-allocation* story, not a free-list
fragmentation one. The instruments are G1's own: the collection-set selection,
the humongous path (`is_humongous`, `pin_region_for_addr`) and whether a
completed concurrent mark cycle is reclaiming dead Old/humongous spans —
`last_ditch_reclaim` exists precisely because it is only a *finished* cycle's
cleanup that frees them.

Rerun it at least three times before drawing any conclusion from a count.

## 2026-08-29 (later): reproduced, and the census says it is NOT to-space exhaustion

Azure Linux, `--Xmx 1g -XX:+UseG1GC`, 900 s cap, `CRATONVM_GC_STATS=1`, one
binary (`dev@a94842f04`). **Both faces reproduced in two runs**, and they have
DIFFERENT internal signatures — which is the first thing here that the "the
face varies" line can be replaced with:

| rep | rc | secs | real OOM | `[g1][SECURITY V7b]` dangling refs | implausible-header cycles |
|---|---:|---:|---:|---:|---:|
| 1 | 124 (cap) | 900 | **0** | **48 617** | 0 |
| 2 | 1 | 120 | **1** | 129 | 14 (2 × "retain all regions") |

### The OOM face

```text
Caused by: java/lang/OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576)
    at org/h2/mvstore/FileStore.getWriteBuffer / WriteBuffer.<init>
    ... MVStore.commit -> storeNow -> serializeAndStore
```

A **1 MB** request failing on a **1 GB** heap. The collector census from the
same run says why, and it is not fragmentation and not to-space exhaustion:

```text
g1 concurrent mark: skipping gray entry 0x20080000008 (region 988) with implausible
    header — cleanup will retain all regions this cycle
g1 cleanup: implausible gray entry seen during marking — retaining all regions
    this cycle (no in-place frees)
[GC] g1 cycle #62293: kind=young cset_young=0 cset_old=0 pinned_out=5 rset_sources=0
     degraded=jit-pinned-regions-excluded,empty-collection-set
[GC] g1 humongous-eager: spans=0 bytes=0 declined_pauses=16103
```

The chain is: **a corrupt header reaches the marker → the `G1MARK-8` fail-safe
retains every region for that cycle (by design: an incomplete closure makes a
0-live verdict untrustworthy) → the humongous eager reclaim declines 16 103
pauses and frees `spans=0 bytes=0` → collection sets go empty
(`jit-pinned-regions-excluded`) → 62 293 cycles reclaim nothing → the 1 MB
humongous request cannot be served.**

So the heap is not exhausted by live data. **The collector is deliberately
declining to free anything**, because it does not trust what it marked. The
page's "check first" list should start at the corruption, not at the
collection-set selection: `last_ditch_reclaim` and the humongous path are
downstream of a marker that has already given up.

### The other face is a use-after-free the guard names outright

The capped run never OOM'd; it logged **48 617** of these:

```text
[g1][SECURITY V7b] post-evacuation dangling reference: object 0x20045241910 in
    surviving region 46 still points at 0x…, which lies in a freed CSet region
    with no forwarding entry (incomplete remembered set => UAF)
```

48 300 of the 48 617 come from **one object** in region 46; the rest are spread
over seven other regions. `report_dangling_cset_ref` is a hard abort in debug
builds and a loud log in release — so a normal run has the same UAF and prints
nothing. This same signature is what `G1-9` was: the parallel evacuator not
scanning a COMPACT object's reference fields, reachable only from a full VM run.

`CRATONVM_G1_PARALLEL_EVAC=0` is therefore the bisect this page should run
next, and it is running.

### A measurement trap this page's own numbers may be sitting on

`grep -c OutOfMemoryError` over a G1 run's **stderr** is contaminated. The
empty-JIT-publication warning contains the words *"trades the wrong answer for
an OutOfMemoryError"*, so a throttled warning that fired 14 times adds 14 to
that count. Rep 1 above scores `oom=14` by that grep and **zero** by the actual
exception. The inherited figures on this page — "2 `OutOfMemoryError`" and "13
`OutOfMemoryError`" — were produced by the parent page's tooling and should be
re-derived before being compared against anything: count
`^Exception in thread` / `java.lang.OutOfMemoryError` in **stdout**, or match
`OutOfMemoryError:` with its colon.

`emptypub` is worth its own note: the warning is throttled to the first eight
then powers of two, so 14 printed lines means **at least 512** pauses ran
against a JIT root publication the conservative scan could not fill.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g -XX:+UseG1GC \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of, and the A/B that separated the two.
