# `-XX:+UseG1GC` fails `TestKillProcessWhileWriting` — a G1 `OutOfMemoryError` with no arena failure

## ADDENDUM 2026-08-30: the OOM face is FIXED, the 48 617 dangling references were never a rate, and the class still does not pass

Three separate corrections, in decreasing order of how much they change what
this page says.

### 1. The OOM face is fixed, and the fix is one gate that answered one bit too few

`plausible_mark_scan_target` returned a single bool, and ANY refusal set
`mark_saw_implausible`, which makes `cleanup` retain every region -- no
in-place frees, no humongous reclaim -- for the whole cycle. That is the chain
this page already traced: retain-all -> `humongous-eager declined_pauses=16103
spans=0 bytes=0` -> `degraded=empty-collection-set` -> a 1 MiB
`ByteBuffer.allocate` fails on a 1 GiB heap.

**Two conditions were reaching that one bit, and only one of them is evidence
of anything.** With the new `CRATONVM_G1_DBG_GRAY_PROV=1`, which records where
each gray-set entry was pushed from, a 32 s run says:

| refusal | count | what it is |
|---|---:|---|
| `NotAllocated` | **2 226** | at/beyond the region's cursor, or in a type that holds no object starts |
| `TornHeader` | 23 | inside the allocated prefix, header does not decode |

`NotAllocated` is provably not a live object: `cursor` only grows within an
incarnation, so every live object satisfies `off + size <= cursor`. And it is
ORDINARY, not corruption -- freed regions are deliberately no longer scrubbed
(G1AUD-10), so a recycled region still holds its previous incarnation's bytes
above the new cursor, and SATB retention means the marker legitimately scans
objects that died mid-cycle whose referents were freed with them. Every one of
the 2 226 names a live object's reference slot as its pusher
(`prov=scan-child-legacy<-0x20042fac8d8`), not a stale worklist entry.

The gate now returns `NotAllocated` vs `TornHeader`; only the latter impugns
the cycle. MEASURED, one binary, `CRATONVM_G1_MARK_OOB_FAILSAFE=1` as the
control, 900 s:

| arm | retain-all cycles | real OOM |
|---|---:|---:|
| failsafe restored (pre-fix behaviour) | 17 | 0 |
| split (default) | **1** | 0 |

and **0 real `OutOfMemoryError` across three default runs**, against this
page's 2 of 4. The `Caused by: java/lang/OutOfMemoryError: Java heap space
(ByteBuffer.allocate 1048576)` face is gone.

### 2. The 48 617 dangling references are SIX holders, and the count was never a rate

The V7b verifier walks every surviving region LINEARLY on a rotating budget, so
it re-reports the same unrepaired slot on every pause that reaches its region.
Deduplicated by `(holder, target)`, this page's 48 617 lines are **6 distinct
holders and 26 distinct targets**. The report now prints one line per distinct
pair and carries `distinct=` and `total=`, so the two numbers stay separable.

Two further cautions the page should carry:

* the verifier inspects DEAD objects too -- it walks the region, not the live
  set -- and a dead object pointing at a dead CSet object that was correctly
  not evacuated is not a UAF at all;
* `holder_marked` is now reported rather than assumed, and `marking_active` is
  reported beside it **because the first attempt at this datum was vacuous**:
  all 2 946 pairs in one run read `holder_marked=false` with
  `marking_active=false`, i.e. the bitmap was cleared and could not answer.
  A mark-bit read outside a mark cycle is not a liveness verdict.

### 3. THE FAILURE MODE MOVED, and that is a cost this page must carry

The retain-everything fail-safe was not fixing anything -- it was MASKING, by
never freeing. Removing the mask lets evacuation run again, and the corruption
that was previously latent now bites:

| arm | runs | outcome |
|---|---|---|
| this page's original default `-XX:+UseG1GC` | 4 | `124` cap, `1` OOM, `124` cap, `1` OOM |
| with the refusal split (default) | 4 | `124` cap, **`139` SIGSEGV**, `124` cap, **`139` SIGSEGV** |

Zero `OutOfMemoryError` and zero retain-all cycles in the new arm, and two hard
segfaults where there were none. **A caller should read that as a trade, not as
an improvement**: the class failed before and fails now, and a SIGSEGV is a
harder failure than a capped run.

It is shipped default-ON anyway, for two reasons that should be re-examined if
a G1 workload regresses. First, the blast radius is bounded -- G1 is not the
default collector, so only an explicit `-XX:+UseG1GC` arm is affected. Second,
the alternative is keeping a fail-safe that fires on an ordinary condition and
buys its silence by never reclaiming, which cannot be a resting state.

`CRATONVM_G1_MARK_OOB_FAILSAFE=1` restores the old behaviour on the same
binary, and `mark_oob_gray_skips` is reported at cleanup so the fail-safe that
stopped firing does not become one nobody can see.

### 4. The class still does not pass, and what remains is named

So the OOM face is gone and the corruption faces are not (see 3 above for the run
table and what that trade costs). The logs name the source outright, and
these are the lines to start from:

```text
[g1] worklist-scan[object]: REJECTED a non-object candidate (#134217728):
    holder=0x20045f1bf90 class_id=0 kind=Object slot=2200197358352
[g1] evacuation ref-scan CLAMPED a holder's element walk (#16384):
    obj=0x20045f1c140 declared=2162688 room=1971
```

Both are throttled to powers of two, so `#134217728` means **at least 2^27**
non-object candidates rejected and `#16384` at least 2^14 clamped walks. A
holder with `class_id=0` and a slot index of 2.2e12 is not an object; something
is walking memory that is not a live object and interpreting it as one. That is
the next question on this page, and it is upstream of both remaining faces.

## Status

**OPEN. The OOM face is FIXED (2026-08-30) but the FAILURE MODE MOVED to SIGSEGV -- read the addendum above, section 3, before treating that as an improvement. The 48 617 dangling references are 6 holders, not a rate. Split out 2026-08-29** from
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

Azure Linux, `--Xmx 1g`, 900 s cap, `CRATONVM_GC_STATS=1`, one binary
(`dev@a94842f04`), four G1 reps plus the default-collector control. **The "face
varies between runs" line can now be replaced with a rule**: there are exactly
two faces, they are mutually exclusive, and each has its own signature.

| rep | arm | rc | secs | real OOM | `[g1][SECURITY V7b]` dangling refs | implausible-header cycles |
|---|---|---:|---:|---:|---:|---:|
| 1 | `-XX:+UseG1GC` | 124 (cap) | 900 | 0 | **48 617** | 0 |
| 2 | `-XX:+UseG1GC` | **1 (OOM)** | 120 | **4** | 129 | **14** |
| 3 | `-XX:+UseG1GC` | 124 (cap) | 900 | 0 | **50 747** | 0 |
| 4 | `-XX:+UseG1GC` | **1 (OOM)** | 749 | **4** | 275 | **21** |
| 5 | **default collector** | **0 (PASS)** | 441 | **0** | **0** | **0** |

**The OOM face and the implausible-header cycles occur together, 2 for 2, and
never alongside the 50 000-dangling-reference face; the cap face is the exact
complement.** The default collector shows zero of all three and passes — which
is the control this page asserts and had not measured on this tip.

So G1 corrupts this workload on every run. Whether that surfaces as an
`OutOfMemoryError` or as a 900 s livelock is decided by *which* corruption the
collector notices first, not by whether corruption happened.

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

The capped runs never OOM'd; they logged **48 617** and **50 747** of these:

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

`CRATONVM_G1_PARALLEL_EVAC=0` was therefore the obvious bisect. **It is
refuted, and it fails in the direction that rules the hypothesis out rather
than merely failing to confirm it:**

| arm | rep | rc | secs | `V7b` dangling refs | real OOM |
|---|---|---:|---:|---:|---:|
| parallel evac ON (default) | 1-4 | 124/1/124/1 | 900/120/900/749 | 48 617 / 129 / 50 747 / 275 | 0/4/0/4 |
| `CRATONVM_G1_PARALLEL_EVAC=0` | 1 | **139 (SIGSEGV)** | 652 | **95 332** | 0 |
| `CRATONVM_G1_PARALLEL_EVAC=0` | 2 | 1 (OOM) | 508 | 23 276 | 4 |

**2/2 still corrupt with the parallel evacuator off**, one of them with a hard
segfault — the third face this family is known for. The dangling-reference
count does not drop into the noise; it stays in the same 10⁴-10⁵ band the
default arm produces (95 332 and 23 276 against 48 617 and 50 747), so the
counts are not a discriminator in either direction and only the pass/fail is.

So the incomplete remembered set is not the parallel evacuator's
compact-object stride: it is in the path both evacuators share. The guard's
own wording is the thing to take literally — *"incomplete remembered set"* —
and the RSet is built before either evacuator runs.

That also retires the `G1-9` resemblance. The signature matches; the cause
does not.

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
