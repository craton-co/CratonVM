# G1: something writes EIGHT BYTES at a live object's base, mid-pause — the `TestHostConfigAutomaticDeploymentXmlExternalWarXml` residual

| | |
|---|---|
| **Status** | **OPEN, and now known to be TWO defects.** The reports are REAL corruption, not a desynced walk: `grid_closes_on_cursor=true` on 11 of 11, the walk stepping 8623 whole objects onto the exact region cursor (2026-09-07 section). One run produced BOTH a body-cell population (6 sound `class_id=64` objects, corrupt slots) and a header population (8 arena-pointer holders), at disjoint addresses. `word0_plausible_ptr` has a false-positive class, so this page's 19-of-19 statistic needs re-taking. The corrupt-header family no longer reproduces on H2 as of dev's 2026-09-06 evacuation fixes: ablating the four of them together brings it back (7 arena-pointer holders and a SIGSEGV in ~400 checkpoints, against 0 in ~36 000 with them on) -- see the 2026-09-07 section. Which of the four, and whether the Tomcat-side reports were corruption or a desynced walk, are both still open. The producer was never identified directly, and as of 2026-09-06 it is known NOT to be any of the six flat walks: screening two of them moves the reports to the others at an unchanged rate, and `CopyWatch` clears the copy path. The origin is upstream of everything this page instruments. The title's "eight bytes" is contradicted by the H2 population measured 2026-09-06 -- see that section; treat the size as unsettled. What this page adds is that the several Java-visible faces are ONE thing, that the thing lands at a live object's base during a pause, and that three of the screens reached for it are blind, note-only, or absent. Four guards and four diagnostic fields landed; the crash survives all of them. |
| **Scope** | The corrupt-cell REPORTS are G1 only (the walks are G1's). The WORKLOAD failing is not: at -Xmx256m `TestMVStoreTool` fails on CratonVM under G1 (OOM / SIGSEGV / `BufferOverflowException`) and under ZGC (`OutOfMemoryError ... native reference array of length 14053`, after 589 s in the create phase), where HotSpot passes rc=0 on the same classpath. Do not let this page's scope absorb that. G1 detail: Measured on `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml`, Windows, jar-first classpath, `-Xmx2g -XX:+UseG1GC`. The same corrupt-cell family is on record from `org.h2.test.store.TestMVStoreTool`. |
| **Left behind by** | `g1-parallel-evacuator-had-none-of-the-serial-arms-header-screens` (2026-09-05), whose own "What is NOT closed" section names this class. |

## The faces are one defect

The class fails four different ways on this host, and the shapes are not
independent:

| face | where it is raised | what it means |
|---|---|---|
| `OutOfMemoryError: Java heap space` | Java | the dominant one — see §Why it OOMs |
| `ClassCastException: class <unknown> cannot be cast to class java.lang.String` | **JIT** checkcast helper | `<unknown>` is `get_class(obj_class_id)` returning `None`. The message has **no** module/loader parenthetical, which is what distinguishes `vm/src/jit/helpers.rs`'s own `format!` from the interpreter's |
| `ClassCastException: class java.lang.Object cannot be cast to class …FrameworkMethod (java.lang.Object is in module java.base …)` | **interpreter** checkcast | it HAS the parenthetical. `java.lang.Object` here is the all-zero header a prematurely-freed block leaves behind — `memory/reclaim_guard.rs` says so in as many words |
| `EXCEPTION_ACCESS_VIOLATION` | native | whatever reads through the same slot next |

All four are a reference slot naming memory that is not what the slot's type
says it is. The two `ClassCastException` shapes are the same event read at two
different stages of block reuse (`reclaim_guard`: "a block freed while still
referenced only reads as `java.lang.Object` until the allocator reuses it;
afterwards the same stale reference sees a *valid* object of an unrelated
class").

## What the corruption physically is

`ObjectHeader` is `class_id`(4) + `shape`(4) + `mark_word`(8) — so **the
`class_id`/`shape` dword pair IS the object's first eight bytes**, and the mark
word is at +8.

Recombining the two dwords of every corrupt holder this session measured gives
a pointer into the collector's own arena, and the mark word beside it is
well-formed (a plausible quartet, a `gc_age` a copy incremented):

```
holder=0x2f056b807b0 class_id=2461533248 num_slots=752
                     header_word0=0x000002f092b80440 word0_plausible_ptr=true
                     mark=0x1000000000000000 gc_age=1 is_compact=false
```

`num_slots` 752 = 0x2F0 over `class_id` 2461533248 = 0x92B80440 is the single
word `0x000002F092B80440`, and that run's arena is at `0x2f0……`.
**19 of 19 holders in one run had `word0_plausible_ptr=true`.**

An eight-byte write at the base, with the mark word untouched, is a much
narrower statement than "the header is corrupt": a sixteen-byte legacy `Value`
write would have taken the mark word out too.

### Why it OOMs — WITHDRAWN 2026-09-06, the OOM is a separate defect

**This section was wrong, and the correction is measured.** It said:

> `object_total_size` reads `num_slots` from that same dword … so a small
> object is sized at `16 + 752*16 ≈ 12 KB` and `evacuate` copies twelve
> kilobytes for it, every pause, until to-space is gone. The OOM is not a
> separate problem from the ClassCastExceptions.

It is a separate problem. The OOM is
`g1-promotion-tlabs-took-a-fresh-region-per-worker-per-pause` (FIXED,
`CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST`): every evacuation worker's promotion
TLAB took a whole fresh Free region in every pause and abandoned it partly
filled, so the old generation grew by **one region per worker per young pause**
— measured `+23` per pause against 23 workers, `+4` under
`CRATONVM_G1_WORKERS=4`, `+1` on the serial arm — until `free_regions` hit 0.
Total bytes ever copied in such a run: 0.64 GB, into 2.03 GB of Old regions.
Nothing reclaimed them: every pause was `YoungOnly` and the first `Mixed` pause
came AFTER the `OutOfMemoryError`, on the last-ditch path, where it handed back
1100 regions at once.

That also settles the kill switch below. `CRATONVM_G1_PARALLEL_EVAC=0` made the
class healthy because the SERIAL evacuator's `alloc_in_type_locked_scan` scans
existing non-CSet regions of the destination type before claiming a Free one —
30 Old regions for the whole class against 2026. The 0/10-vs-5/5 split was this
defect, not the eight-byte write.

## Which walk trips over it

`#[track_caller]` on both flat-walk entry points, one run, 19 holders:

| caller | count |
|---|---:|
| `g1.rs:1752` — `SharedEvac::process_object` (parallel evac ref-scan) | 12 |
| `g1.rs:10684` — `collect_outgoing_cross_region_edges` (Phase-4 rset rebuild) | 5 |
| `g1.rs:19663` — `EvacResolve::resolve` | 2 |

These are the walks that *read* the damage. None of them is the writer.

## CORRECTION 2026-09-06: it is NOT `SharedEvac`, and that was measured

The section below inferred "the write happens inside a pause, after the copy"
from the fact that candidates screen clean and holders do not. **A copy-watch
refutes it.**

`CRATONVM_G1_EVAC_COPY_WATCH=1` records every to-space copy's first header word
as `evacuate` makes it — the normal-copy CAS winner AND the
evacuation-failure self-forward — and re-reads them at three checkpoints:
after the parallel closure, after the serial self-forward drain, and at the
START of the next pause (dropping entries whose region has since been recycled).

| checkpoint | copies checked | changed |
|---|---:|---:|
| after the parallel closure | 374k, 401k, … (30 pauses) | **0** |
| after the serial self-forward drain | same | **0** |
| at the start of the next pause | 447k / 446k / 504k … (3 188 checkpoints) | **0** |

Not one object that `SharedEvac` copied or self-forwarded ever had its first
word rewritten — inside the pause or between pauses, in runs that went on to
OOM. **The eight-byte write is not in `SharedEvac`.** Everything below about
what the corruption physically IS still stands; only the attribution was wrong.

## What IS wrong in `SharedEvac`: it declares to-space exhaustion while to-space is free

The kill switch (parallel 0/10, serial 5/5) has a different explanation, and it
is a one-line difference between the two allocators:

* the serial `alloc_in_type_locked_scan` scans **every existing non-CSet region
  of the destination type** and bump-allocates into a partially-filled one, and
  only then claims a Free region;
* `SharedEvac::tlab_alloc` can only ever take from `pool` — the Free regions
  reserved before the dispatch. When the pool is gone it returns `None`, which
  is the **evacuation-FAILURE** signal: self-forward, keep the CSet region, run
  `retry_after_evacuation_failure`.

Measured, same census, same class:

```
[g1] parallel evacuation EXHAUSTED ITS POOL (#1): dest_type=Old size=48
     pool_len=1 — but 2017 of 2025 non-CSet Old regions still have room for it.
```

A **forty-eight byte** promotion failing with 2017 usable regions. Same binary,
arms interleaved, parallel against `CRATONVM_G1_PARALLEL_EVAC=0`: **14 such
reports in the parallel arm, 0 in the serial arm.**
That is why the two arms diverge: one of them spends the pause copying and the
other spends it failing to copy, and the failure path is the fragile one.

`CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST` (default ON) gives the parallel arm the
same fallback: on pool exhaustion, one object's worth of space in an existing
non-CSet region of the destination type. It is a direct `bump_alloc` and
deliberately NOT a TLAB — a TLAB owns its region's cursor and `retire_tlab`
STORES it, which would discard a shared bump — and it skips the pool and the
CSet for that reason and for Phase 5's.

A SECOND census, again one binary with the arms interleaved, this time the
fallback off against on, three repetitions each:

| arm | pool-exhaustion reports |
|---|---|
| `CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST=0` | 16, 19, 14 |
| default | 9, 0, 0 |

and the `compact reference walk REFUSED a field offset past the holder's own
body` count goes 8 with the fallback off to 0 with it on. The corruption signal
tracks the exhaustion, which is the causal chain this page was looking for.

(These are two SEPARATE censuses and the numbers must not be spliced: the
parallel-vs-serial 14/0 above and the off-vs-on table here were measured in
different runs, and an earlier revision of this page quoted "16 … and 0 in the
serial arm" by taking one number from each.)

The clincher is what the exhaustion report says on each arm. It prints how much
room the serial arm WOULD have found at that instant, and after the fallback has
already been tried:

| arm | report |
|---|---|
| fallback OFF | `dest_type=Survivor size=32 pool_len=28 — but 23 of 24 non-CSet Survivor regions still have room` |
| fallback ON | `dest_type=Survivor size=176 pool_len=0 — but 0 of 2 non-CSet Survivor regions still have room` |

**Off, it is failing a 32-byte evacuation with 23 of 24 regions usable and 28
regions still in its own pool. On, it only reports once the heap is genuinely
full.** The parallel evacuator is no longer manufacturing to-space exhaustion;
what is left is real.

**It does not make the class pass.** Neither arm is healthy yet — but "the
remaining failures are genuine heap exhaustion at `-Xmx2g`", which is what this
line used to say, was an assumption rather than a measurement. **HotSpot
runs the same eight test cases in 18 s at `-Xmx512m` with an 18-24 MB live set,
flat.** The exhaustion was manufactured; see the WITHDRAWN section above and
`g1-promotion-tlabs-took-a-fresh-region-per-worker-per-pause-FIXED-20260906`.
Pool exhaustion was real, is fixed, and is not the whole story — in the OOM
census it is a CONSEQUENCE, its first report landing only once `free_regions`
was already down to single digits.

## The corruption happens INSIDE a pause, after the copy

`IMPLAUSIBLE legacy header at ref-slot-candidate` is **0** in the runs whose
holders are corrupt. Candidates are screened before `evacuate` dereferences
them, and they pass; the holders that later read as corrupt are the *to-space
copies those same candidates became*. So the pointer is written at the base of
an object that was sound when it was copied, at some point before the same
pause scans it.

**Superseded — see the CORRECTION above.** The copy-watch shows no such write
happens to anything `SharedEvac` produced. The complementarity the reasoning
rested on (corrupt holders and implausible candidates never co-occur) is still
a real regularity; the conclusion drawn from it was not.

## Three screens that could not see it

1. **`note_implausible_legacy_header`'s class-id band is blind to half this
   population by construction.** It exempts `class_id >= 0x8000_0000` because
   `alloc_lambda_proxy_id` counts up from there — and a 64-bit heap pointer's
   LOW dword has bit 31 set about half the time. The exemption sits exactly
   over the values a pointer's low half takes. **Fixed here:** the screen now
   also asks whether the recombined pair is a pointer into this collector's
   arena, which needs BOTH dwords to conspire and cannot go stale the way an
   assumption about the class-id space did. It fires 14-16 times per run where
   the band fired zero.
2. **The reference-slot route ran that screen and threw the answer away.** The
   ROOT route is `candidate_header_is_plausible && !note_implausible_legacy_header`
   (`addr_is_followable_object`, load-bearing since 2026-09-02);
   `evacuation_candidate_is_an_object` called the same function purely to log
   and then `return true`. Its own comment claimed it was "the same second look".
   **Wired up here, but OPT-IN** — see §What was NOT turned on.
3. **The compact reference walk had no bound of any kind.** The legacy arm got
   `max_slots` on 2026-08-26; the compact arm — the one that writes an
   **eight-byte pointer** (`write_flat_object_reference(.., compact = true)`) —
   was never bounded. **Fixed here:** a field offset that leaves the body its
   own layout declares is skipped and counted. It has not fired yet, so it is a
   guard rather than a fix.

## The clamp diagnostic was reading two different things as one

`EVAC_HOLDER_CLAMPED` is the counter a reader would reach for first, and it
could not be read: it fires on a healthy run and its message is the same line
for opposite verdicts. Split by the bound it was computed with, one run:

* **`bound=RegionEnd`** (the parallel arm's), 13 events, `off` in
  `0xfff38..0xfffd8` — every one within the last 200 bytes of a 1 MiB region,
  i.e. addresses whose declared object would cross the region boundary;
* **`bound=Cursor`** (the serial arm's), 10 events at offsets scattered across
  the region (`0x1040`, `0x38cf8`, `0xbe5b8`, `0xfef68`, …), and **nine of the
  ten carry the identical `paired=0x000000030000028a`** — class 650, three
  fields, an entirely ordinary object.

Nine identical ordinary-looking holders clamped from `declared=3` to `room=1`
at unrelated offsets is not nine corruptions. It is the signature of the clamp
measuring a **compact** body with the legacy 16-byte ruler:
`for_each_flat_object_reference_capped` ignores `max_slots` on the compact arm,
so the cap never restricted anything — it only mis-measured. A
`holder_is_compact` field was added to settle this; **the reports carry it now,
so the next reader does not have to re-derive it.**

The reason this mattered enough to chase: a clamp that bites a SOUND holder
drops the reference fields it truncates, their referents are never evacuated,
and they dangle — which is this very defect. Ruling that out was necessary
before trusting any of the rest.

## What was NOT turned on, and why

`CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` is **opt-in**, not default-on,
despite the parity argument being strong. Two measurements decided it:

* with it on, the run still SIGSEGVs — it refuses 14-16 candidates and does not
  fix what it was reached for;
* a refusal that is WRONG drops a live reference, which manufactures exactly
  the premature-free corruption this page is about.

Defaulting it on would trade an understood failure for an unmeasured one. The
same reasoning is why the compact-arm bound IS default-on: skipping a field
that the holder's own layout places outside the holder's own body cannot drop a
live reference, because no live reference is there.

## The kill switch that makes it go away

The sharpest fact on this page, and the one to start from:

One binary, three arms, interleaved per repetition, idle host:

| arm | PASS | CRASH | OOM |
|---|---:|---:|---:|
| `default` — parallel, every screen armed | **0** | 1 | 2 |
| `screenoff` — parallel, the 2026-09-05 screens stood down | **0** | 1 | 2 |
| `serial` — `CRATONVM_G1_PARALLEL_EVAC=0` | **3** | 0 | 0 |

and across every census this investigation ran (runs killed by a session
restart excluded, not counted as either):

| arm | healthy | unhealthy |
|---|---:|---:|
| parallel | **0** | 10 |
| serial | **5** | 0 |

Fisher exact on 0/10 against 5/5 is p ≈ 3e-4. The two parallel arms are
indistinguishable from each other, which is its own result: **the 2026-09-05
screens change nothing here** — they were the right parity fix for the assert
they were built for and they are not what this class needs.

**The serial arm is not a control that dies early.** It finishes the whole
class in 232-264 s, against 387 s for the one parallel run that reached a JUnit
summary at all — it does the same work, faster, and passes. So this is not
"the serial arm never reaches the depth where the fault happens", which is the
shape that invalidated a neighbouring page's driver-off arm.

**The producer is therefore inside `SharedEvac`**, and the eight-byte write is
a write some parallel worker makes. That is consistent with everything above —
the corruption appears on to-space copies, mid-pause — and it narrows the
search from "GC code" to one module.

### and the caveat that keeps this honest

**The corrupt-cell family is on BOTH arms.** One of the three passing serial
runs reported 17 corrupt holders. So the corruption is not the discriminator —
the OUTCOME is. Either the parallel arm carries a second defect that decides
whether the corruption is fatal, or the same corrupted header is only acted on
destructively when a parallel worker reaches it (it is the arm that both sizes
a copy from the header and rewrites slots through it, concurrently).

A second regularity, across all thirteen runs of this investigation, is worth
carrying forward because it will otherwise be re-derived: **corrupt HOLDERS and
implausible CANDIDATES never co-occur.** Every run with `hold > 0` has
`ref-slot-candidate = 0`, and the single run with `ref-slot-candidate = 10` has
`hold = 0`. Two populations, never together — which is the strongest evidence
on this page that the damage lands on holders (to-space copies, after the copy)
rather than on the candidates the screens inspect.

It does NOT follow that the parallel evacuator should be turned off: it is the
default for throughput reasons, this is one class on one host, and switching
collectors' arms on the strength of nine runs would be trading a measured
defect for an unmeasured regression everywhere else.

## The H2 population says it is NOT an eight-byte write

The `word0_plausible_ptr` test above was applied to the OTHER workload this
family is on record from -- `org.h2.test.store.TestMVStoreTool`, `-Xmx256m`,
`CRATONVM_G1_JIT_MARK_DRIVER=1` -- over **118 distinct corrupt holders from 13
runs** (deduplicated on `(holder, class_id, num_slots, mark)`; a holder is
counted once however many of its cells the walk rejected). "Arena pointer"
here is: 8-byte aligned, same 4 GiB window as the holder, within 1 GiB of it.

| what the 16-byte header holds | holders |
|---|---:|
| **BOTH words are arena pointers** | **53** |
| `word0` only -- the shape this page describes | 25 |
| `mark` only | 7 |
| `mark` is a SELF-forward (`holder` tagged 3) | 11 |
| neither | 22 |

**The mark word is not untouched.** This page's central inference --

> An eight-byte write at the base, with the mark word untouched, is a much
> narrower statement than "the header is corrupt": a sixteen-byte legacy
> `Value` write would have taken the mark word out too.

-- holds for 25 of 118 holders here. In the largest population the write took
**both** words out, which is the case that inference excludes. On the Tomcat
class the mark survived 19 of 19 times; on H2 it survives 25 of 118. Two
workloads, opposite majorities, so "eight bytes" is a property of the Tomcat
sample and not of the defect.

That matters because the eight-byte framing is what narrows the suspect list to
the two eight-byte reference writes in the next-step section. If sixteen bytes
go in half the population, a whole-body overwrite is back on it.

### and the body is foreign too

The strongest evidence for a bulk overwrite was already in this tree, recorded
in `for_each_flat_object_reference_capped`'s own comment and never connected to
the header question: for one corrupt holder the cells the walk then read were
`raw0=0x6f57206f6c6c6548` and `raw1=0x3532363620646c72` -- `"Hello Wo"` and
`"rld 6625"`, the payload of a Java string.

A holder whose HEADER is two foreign pointers and whose BODY is a foreign
string's characters was not hit by a write of eight bytes, or of sixteen. A run
of foreign bytes landed on it, header first. The two header words read as
pointers because they are the source's first two body words; they read as a
plausible quartet and an incrementing `gc_age` in the Tomcat sample for the
same reason -- those are whatever the source had there.

### the deltas, for whoever instruments this next

Over the 53 both-pointer holders, `mark[0:48] - word0` is small in 24 cases and
takes exactly three values there -- `+0x28` (x11), `+0x18` (x9), `+0x50` (x4)
-- with 22 larger and 7 negative. Small object sizes, i.e. two adjacent fields
naming consecutively-allocated objects, which is what the first two reference
fields of a copied body usually are. It is consistent with the bulk-overwrite
reading; it does not on its own prove it, and 29 of 53 are not small.

**What was checked and is NOT the mechanism.** `SharedEvac::evacuate` copies
`obj_size` bytes to a block `tlab_alloc` returned for exactly `obj_size`
(`new_off <= tlab.len`), so an oversized header cannot make that copy run past
its destination -- it exhausts the pool one region per attempt and returns
`None`, which is this page's OOM, not an overwrite. Whatever writes the run of
bytes, it is not that call site overrunning.

## The walks are READERS: screening them moves the reports, it does not stop them

Measured 2026-09-06 on H2 (`TestMVStoreTool`, -Xmx256m, G1, mark driver on),
one binary, one kill switch, 5 interleaved pairs. Arm A ablates a new
word0-is-an-arena-pointer holder refusal on the serial evacuator and on the
Phase-4 fixup; arm B has it on.

| arm | corrupt per rep | serial refusals | phase-4 refusals |
|---|---|---|---|
| A | 13, 13, 0, 0, 21 | 0 | 0 |
| B | 16, 0, 13, 0, 19 | 10, 4, 0, 9, 9 | 12, 11, 11, 14, 12 |

47 against 48, and 8 of 10 runs SIGSEGV in both arms. The screens **engage** --
this is not a vacuous arm -- and the family survives them.

**Where the reports went is the whole result.** `#[track_caller]` on the same
runs:

* `A-1` -- 13 of 13 at `scan_and_evacuate_refs` (the walk arm B screens);
* `B-1` -- 15 of 16 at `scan_source_region_for_cset_refs`, which has no screen;
* `A-5` -- 19 of 21 at `update_object_refs`;
* `B-4` -- spread across five different walks.

Screening one reader moved the population to the next one. There are **six**
flat-walk sites, not the three this page's `#[track_caller]` census found:
`scan_and_evacuate_refs`, `scan_source_region_for_cset_refs`,
`collect_outgoing_cross_region_edges`, `note_humongous_targets_in_region`,
`update_object_refs`, `seed_source_region`.

### what that settles about the producer

A holder that is already corrupt when six independent walks read it was not
corrupted by any of them. This page's search has been aimed at the walks; the
walks are readers.

The write they perform past a holder's end is real and worth stopping on its
own terms -- `update_object_refs` reaches
`for_each_flat_object_reference_trusting_header`, whose own doc says it "bounds
it by nothing", and one refusal in these runs names a holder declaring 102736
legacy slots: a 1.6 MB body in a 1 MiB region, rewritten cell by cell with
forwarding addresses. But it is an AMPLIFIER. It explains why corruption
arrives in bursts within one second and why victims' first two words are two
arena pointers one object-size apart. It does not explain the first one.

**The origin is upstream of every walk on this page and is still unfound.**

### and the copy watch says it is not the copy either

`CopyWatch` (`CRATONVM_G1_EVAC_COPY_WATCH=1`), run on H2 for the first time:
**zero** first-word rewrites over ~700k to-space copies, at all three of its
checkpoints -- before the pause, after the parallel closure, after the serial
drain -- in runs that reported 13 and 17 corrupt holders. So the victims are
not sound to-space copies overwritten after the copy, which is the inference
the top of this page rests on.

## CLOSED 2026-09-07: dev's four evacuation fixes close the corrupt-header family

One binary, all four of dev's 2026-09-06 evacuation fixes default-ON and
individually ablatable, interleaved ABBA on H2 `TestMVStoreTool` (-Xmx256m, G1,
mark driver on, copy watch on):

* **A** -- `PARALLEL_EVAC_RESUME_DEST=0 RETIRE_FORWARDS_LATE=0 REEVAC_GUARD=0
  PARALLEL_EVAC_SHARED_DEST=0`
* **B** -- default

| arm | reps | copy-watch checkpoints | holders with `word0` an ARENA POINTER | crashes |
|---|---:|---|---:|---:|
| A (fixes off) | 3 | 151, 133, 121 | **7** | 1 SIGSEGV at 37 s |
| B (default) | 3 | 12301, 12064, 11905 | **0** | 0 |

The refused holders in A are the genuine shape, not the array false-positive
class -- `holder=0x252662c9cf0 paired=0x000002525cd7d290`,
`holder=0x2114fe1ac60 paired=0x000002114040ba70`, both `paired` values landing
in that run's arena. A-2 refused the SAME `paired=0x2525cd7d290` under two
holders at `gc_age=1` and `gc_age=2`: one corrupted object, copied by two
successive evacuations, which is the pattern this page recorded from the start.

### which of the four: `PARALLEL_EVAC_RESUME_DEST`, alone

Per-flag ablation, one flag off per arm, everything else default, two
repetitions, same binary and workload:

| flag turned OFF | rep 1 | rep 2 | arena-pointer holders |
|---|---|---|---:|
| **`CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST`** | **died at 32 s** | **SIGSEGV at 63 s** | **11, 15** |
| `CRATONVM_G1_RETIRE_FORWARDS_LATE` | timeout 420 s | timeout 420 s | 0, 0 |
| `CRATONVM_G1_REEVAC_GUARD` | timeout 420 s | timeout 420 s | 0, 0 |
| `CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST` | timeout 420 s | timeout 420 s | 0, 0 |
| none (control) | timeout 420 s | timeout 420 s | 0, 0 |

2 of 2 with `RESUME_DEST` off, 0 of 2 for each of the other three and for the
control. One variable.

### confirmed on THIS class, not just on H2

The attribution above was measured on H2. Re-run here, jar-first classpath,
`-Xmx2g -XX:+UseG1GC`, `CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST=0`:

| rep | outcome | corrupt cells | arena-pointer holders |
|---|---|---:|---:|
| 1 | SIGSEGV at 256 s | 1 | 1 |
| 2 | timeout at 901 s | 13 | 1 |

and the refused holder has the H2 population-A shape exactly --
`holder=0x1ddddc57228 word0=0x1ddb62a0e58 mark=0x…1ddb62a0e78`: BOTH header
words are arena pointers, 0x20 apart. Same defect, same shape, both workloads,
same single flag.

### the grid-closure question is still open HERE, and that is an instrument gap

`grid_closes_on_cursor` was added to settle whether these reports are corruption
or a desynced walk. It produced **0 verdicts in both runs above**, against 14
corrupt cells. The drain that emits it is wired into three pause bodies, and
this class's reports arrive from five walks --
`SharedEvac::process_object`, `seed_source_region`,
`collect_outgoing_cross_region_edges`, `verify_no_dangling_into_cset_within`
and one attributed to `is_collectable_region_type` -- on a pause path that does
not reach the drain at `-Xmx2g`.

So the question this page most needs answered is unanswered on the workload
whose evidence it rests on, and the reason is the instrument, not the defect.
On H2 the same drain emits 8 verdicts a run. Wiring it into the remaining pause
paths is the next step for anyone picking this up; it is small, and it is the
LAST thing between this page and knowing whether its census was corruption at
all.

### so the producer is the EVACUATION-FAILURE path

`RESUME_DEST` is what lets a parallel worker bump into an existing non-CSet Old
region instead of only ever claiming a fresh one from `pool`. With it off,
`tlab_alloc` returns `None` as soon as the pool is gone -- **while to-space is
still free**, which is the "forty-eight byte promotion failing with 2017 usable
regions" this page already measured -- and `None` is the evacuation-FAILURE
signal: self-forward, keep the CSet region, `retry_after_evacuation_failure`.

That is the fragile machine this page called out and never connected to the
corruption. The corrupt headers are produced there, not by any walk, not by the
copy, and not by `SharedEvac`'s normal path -- which is why the copy watch was
clean at every checkpoint while holders kept appearing, and why corrupt holders
and implausible CANDIDATES never co-occurred.

The fix is already default-ON. What is NOT done is hardening the failure path
itself: it is still reachable under genuine exhaustion, and it still produces
this shape when it runs.

### why the depth difference does not void this

The A arm is ~100x shallower, because with the fixes off the collector dies
fast -- normally the "control that dies early" shape that invalidates an
arm. It does not here, because **the asymmetry runs the right way**: the arm
with far FEWER opportunities produced ALL of the defects. Fixes off, 7 corrupted
headers and a SIGSEGV in ~400 checkpoints; fixes on, zero in ~36 000. A control
that dies early can only manufacture a false NEGATIVE, and the negative is in
the arm that ran 100x longer.

Corroborating, on the default arm before the ablation: four reps at 7959, 7094,
1394 and 4749 pauses, **0 corrupt cells and 0 refusals** in every one. The
family used to appear within a couple of minutes and a few hundred pauses.

### what this does NOT say

* **Not which of the four.** They were ablated together; a per-flag ablation is
  what attributes it.
* **Not that the workload passes.** Every default-arm rep is a TIMEOUT, not a
  pass. Under G1 with the mark driver this class does ~7959 pauses in 30 minutes
  and never finishes its CREATE phase; HotSpot completes the whole class in
  about 4 minutes, and the same binary without the driver OOMs at 84 s. That is
  a throughput defect and it is not this page's.
* **Not that the reports were corruption rather than misparse.** That question
  (`grid_closes_on_cursor`, added for it) never got an answer, because after the
  fixes there are no reports left to classify. It stays open against the
  Tomcat-side reports, which this page's own census collected.

## ANSWERED 2026-09-07: the reports are REAL CORRUPTION, and the grid proves it

The question this page could not settle -- are the corrupt-cell reports evidence
of corruption, or artefacts of a walk that lost the object boundaries? --
is answered. `grid_closes_on_cursor`, run on THIS class:

| | |
|---|---:|
| verdicts with `grid_closes_on_cursor=true` | **11** |
| verdicts with `grid_closes_on_cursor=false` | **0** |

and the closure is not marginal:

```
grid_closes_on_cursor=true  grid_walked=8623  grid_ended=0xc27f0
source=r115/Survivor/off=0x92ff0/cursor=0xc27f0
grid=OBJECT-START idx=5357
```

The walk stepped **8623 whole objects** from the region base and ended at
`0xc27f0`, which IS the region cursor, exactly. A walk that had crossed a wrong
size could not land there. So the boundaries are the allocator's, the holder is
a real object start, and every `grid=OBJECT-START` on this page can now be read
at face value rather than as a possible tautology.

### and the corruption is in object BODIES, not only headers

All six distinct holders in that run are `class_id=64 num_slots=18`,
`gc_age=6` -- sound, long-lived objects, in a region whose grid closes
perfectly. Their HEADERS are fine (`header_word0=0x0000001200000040` is just
`(18<<32)|64`). What is corrupt is a CELL in the body, `slot_index=6` of 18,
read by `SharedEvac::process_object`.

That is a DIFFERENT population from the arena-pointer header corruption, and
the same run produced both: 11 corrupt body cells in 6 sound objects, and 8
holders refused because their first word IS an arena pointer -- disjoint
addresses. This page has treated them as one defect. They are two, they
co-occur, and only the second is a header write.

### `word0_plausible_ptr` has a false-positive class

The field reports `true` for `header_word0=0x0000001200000040`, which is an
ordinary `class_id=64 / num_slots=18` header and not a pointer into anything.

Verified rather than eyeballed: `holder_word0_arena_pointer` computes the SAME
predicate over the SAME arena bounds in the SAME run, and it did not refuse
that holder -- while it did refuse eight others. Two independent computations
of one predicate disagree on this input, so the field is wrong here.

**This page cites `word0_plausible_ptr` TRUE on 19 of 19 holders as its
evidence that the first word is a pointer.** That statistic needs re-taking
against the arena test that fires a refusal, not against this field.

### reproduction rate, for whoever runs it next

1 run in 6 on an idle host (fat LTO). Three earlier runs on a thin-LTO build of
the same tree gave 0, which is consistent with the ~1-in-3 rate this page
already records and is NOT evidence that thin LTO masks it. Budget six or more
reps before reading a zero as anything.

## The next step

Everything this section used to say has been measured and closed. Kept as a
list of what NOT to re-run:

* ~~"Start inside `SharedEvac`"~~ -- the copy watch clears it on both
  workloads, 0 rewritten of ~374k/401k (Tomcat) and ~700k (H2) copies at three
  checkpoints. See the CORRECTION section.
* ~~`write_flat_object_reference(.., compact = true)`~~ and ~~the array arm's
  `ptr::write`~~ -- these are in the WALKS, and the walks are readers: screening
  two of them moves the reports to the other four at an unchanged rate (47 vs
  48, screens engaging). See the READERS section.
* ~~a to-space write-watch~~ -- built (`CopyWatch`), run, reported above.
* ~~the OOM~~ -- closed separately: the parallel evacuator took a whole fresh
  Old region per worker per pause, so Old grew by the worker count whatever was
  promoted.

### the question that now comes FIRST

**Are the corrupt-cell reports evidence of corruption at all?**

This page has treated every report as a corrupted object. One sample says that
needs proving, not assuming:

    0xb0530  prev header (cid=689, slots=3, compact, size=0x28)
    0xb0548  0x00000e800111e5c8   hi=0xe80   <- inside prev's BODY
    0xb0558  0x00000e8001121d48   hi=0xe80   <- the "holder"'s first word
    0xb0568  0x00000003000002b1   cid=689 slots=3   <- a NORMAL header

Two 16-byte pairs of the same shape 16 bytes apart, one of them inside a sound
object's body, both first words sharing the high dword `0xe80` -- which is 3712,
the very `num_slots` the "holder" reports -- and an ordinary header 16 bytes
later. That is what Java DATA looks like to a walk that has already lost the
object boundaries, and `locate_in_object_grid`'s own doc says its verdict cannot
tell the two apart: it strides each object by the size THAT OBJECT'S header
declares, so `grid=OBJECT-START` reports where the walk ARRIVED. One wrong size
upstream misparses every boundary after it and still lands on an "object start"
each time. The doc even names the shape it has seen -- `class_id=0x65676170`,
the ASCII bytes `page`, from an H2 MVStore chunk header.

`grid_closes_on_cursor` (added 2026-09-06) settles it per report: it walks the
region independently and says whether the walk lands EXACTLY on the cursor
having stepped only whole objects.

* **closes** -- the sizes it strode were consistent, the boundaries are the
  allocator's, and the holder really is a corrupted object. The producer hunt
  continues, upstream of every walk.
* **does not close** -- the grid desynced before reaching the address, the
  "header" is a neighbour's data, and that report is an artefact. If this is
  where the population lands, the defect to find is whatever desynced the walk,
  and much of the evidence on this page needs re-reading rather than extending.

Run it before adding anything further to this page. A verdict quoted without it
cannot be believed, including the ones already quoted above.

### if the grid DOES close

Then the remaining unmeasured window is the one nothing here has instrumented:
the object between its allocation and the pause that first walks it. The copy
path is cleared, the walks are cleared, and the header is stable from walk time
to end of pause (measured: the walk-time pairing and the drain-time word at the
holder are bit-identical). What is left is a write that happens while the
mutator runs, to an object no pause has copied -- which `CopyWatch` cannot see,
because it only records copies.

`copied_this_pause=` on each corrupt-holder report is the split: YES sends the
search to the from-space source, `no` says nothing copied that object and the
evacuator is not involved at all.

## Reproduction

Windows, jar-first classpath (the shape the Linux suite uses — see the
classpath note on the 2026-09-05 CCL page):

```
cratonvm.exe --java-home <jdk25> --Xmx 2g -XX:+UseG1GC ... \
  -c <jars-first classpath> org.junit.runner.JUnitCore \
  org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml
```

Roughly one run in three is unhealthy (OOM / FAIL / CRASH); the corrupt-cell
family appears on healthy runs too, so **a PASS is not evidence of absence**.
On Azure the same class PASSes far more often — 12 interleaved pairs there
produced no FAIL on the screened arm — so this is a host-sensitive residual and
the Windows box is the one that reproduces it.
