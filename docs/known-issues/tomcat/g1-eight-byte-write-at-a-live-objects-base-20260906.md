# G1: something writes EIGHT BYTES at a live object's base, mid-pause — the `TestHostConfigAutomaticDeploymentXmlExternalWarXml` residual

| | |
|---|---|
| **Status** | **OPEN.** The producer is not identified. The title's "eight bytes" is contradicted by the H2 population measured 2026-09-06 -- see that section; treat the size as unsettled. What this page adds is that the several Java-visible faces are ONE thing, that the thing lands at a live object's base during a pause, and that three of the screens reached for it are blind, note-only, or absent. Four guards and four diagnostic fields landed; the crash survives all of them. |
| **Scope** | G1 only. Measured on `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml`, Windows, jar-first classpath, `-Xmx2g -XX:+UseG1GC`. The same corrupt-cell family is on record from `org.h2.test.store.TestMVStoreTool`. |
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

### Why it OOMs

`object_total_size` reads `num_slots` from that same dword. A pointer's high
dword is the arena's high dword — 752 on this host, 2048 on another — so a
small object is sized at `16 + 752*16 ≈ 12 KB` and `evacuate` copies twelve
kilobytes for it, every pause, until to-space is gone. The OOM is not a
separate problem from the ClassCastExceptions; it is the same header read by
the allocator instead of by a cast.

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

**It does not make the class pass.** Neither arm is healthy yet — the remaining
failures are genuine heap exhaustion at `-Xmx2g` (and one CRASH). Pool
exhaustion was real, is fixed, and is not the whole story.

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

## The next step

**Find the eight-byte write.** Everything above narrows it to: parallel-evacuator
code, during a pause, at the base of an object already copied into to-space.

**Start inside `SharedEvac`** — the kill switch above says the writer is there.
The candidates worth instrumenting, in order:

1. `write_flat_object_reference(.., compact = true)` — the only eight-byte
   reference write in the walk, now bounded by the layout but not by the
   holder's ALLOCATED extent (a layout resolved for the wrong
   `(class_id, field_count)` is still free to land anywhere inside its own
   declared body);
2. the array arm's `std::ptr::write(slot_ptr as *mut u64, …)` — also eight
   bytes, and bounded only by the REGION, so an array whose `array_length` or
   `element_type` is wrong stamps pointers across its neighbours without ever
   leaving the region;
3. a to-space write-watch: record `(addr, class_id, num_slots)` for every fresh
   copy in a small ring, and on the first corrupt holder report whether it was
   sound when copied. That converts "the corruption happens inside a pause"
   from an inference into a timestamp.

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
