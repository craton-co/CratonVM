# The `ClassId(0)` family — a reference that no longer names what its holder thinks

> **Consolidated 2026-08-03** from two pages that were tracking one defect:
> `bug-h2-blocked-frame-classid0-dispatch-miss.md` (the blocked-frame /
> `Object.hasNext()` face) and
> `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md` (the old-gen
> face). Both old filenames asserted a mechanism their own contents had already
> retracted — the second says so in its first paragraph — which is why the new
> name states only what is actually measured.

## Status
**OPEN.** One defect, four faces, two of them measured to opposite verdicts on
the same question. What is fixed, what is measured, and what is left:

* **Fixed and merged.** An old-gen mark gap (`old_gen_gc`'s root seed had no
  resolution at all for an INTERIOR conservative root), and separately a JIT
  miscompile that bound an `invokevirtual` to the compiled entry of its
  CONSTANT-POOL-resolved method with no receiver guard. The second is not a GC
  bug at all and is written up in
  `../../internal/fixed-suite-bugs/jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`.
  It mattered here twice over: it made this family's only reproduction
  (`TestMultiThread`) fail 100 % of runs in 2-9 s so nothing could be measured,
  and a wrong-object return is **indistinguishable at the reader** from a stale
  reference, so it is a live alternative explanation for every occurrence
  recorded before `12769bb23c`.
* **Still open.** The family reproduces on the fixed binary, on both faces.

## Why this is one page

The old-gen page already establishes that two of the faces are one defect:
`java.lang.Object cannot be cast to X` is what a freed block wears **while it
is still on the free list**, and once the allocator re-serves it the same stale
reference reads a perfectly valid object of an unrelated class. Timing decides
which face you see, not mechanism.

The blocked-frame page's only face that has ever produced a verdict is exactly
that second one. And the old-gen residual is now measured to have **no referrer
anywhere in the heap or the root slice** — so whoever reads it later is not
holding a heap reference either. Both pages therefore describe the same shape:
*something holds an address that no longer names what it thinks it names.*

### What still differs — do not smooth this over

The two faces disagree on one measured question, and the disagreement is the
most useful fact on this page:

| | old-gen face | blocked-frame / clone face |
| --- | --- | --- |
| `reclaimed_hole_at` | **in a free block** — reclaimed | **not reclaimed** |
| old-gen reclamation ring | **hit**, names the original class and the freeing collector | **no record at all** |
| what the reader sees | `Object`→X, or an unrelated live class after re-serve | a live, unrelated `java.lang.Thread` |

So on the old-gen face a collection provably freed something; on the clone face
there is no evidence any collection freed anything. Either the address reached
its holder by a route that never involved reclamation (a wrong-object return, a
stale VM-side cache entry), or the reclamation happened long enough ago that a
1 M-entry ring had wrapped. Both are testable; neither is established.

## The four faces

Count all four when measuring — the family is ~4x more visible than any single
face, and this investigation lost three sessions to chasing the one in a page
title:

1. `NoSuchMethodError` on a `java.lang.Object` receiver (`hasNext()Z`,
   `next()Ljava/lang/Object;`);
2. `CloneNotSupportedException` — `java.lang.Object` is not `Cloneable`, and a
   dispatch that reaches `java.lang.Thread.clone` throws unconditionally;
3. `ClassCastException` — either `java.lang.Object cannot be cast to X` (block
   still free) or an unrelated live class (block re-served);
4. a bare `SIGSEGV` with `slot[rN]` reading eight zero words.

### The 2026-08-03 A/B, on a binary with the JIT confound removed

`TestMVStoreCacheLoop`, one worker per arm, same host and window, `--Xmx 1g`,
`CRATONVM_GC=-moving-young`, JIT on. CTL is `CRATONVM_GC_NO_OLD_INTERIOR_PINS=1`
— the pin disabled, the accounting kept.

| arm | worker-hours | causal (`INTERIOR conservative root` freed/dropped) | reclaim verdicts | `cannot be cast` reaching Java |
| --- | --- | --- | --- | --- |
| FIX | 1.77 | **0** | 1 | 4 |
| CTL | 1.77 | 1 | 3 | 4 |

**Read the CAUSAL column, not the last two.** The mechanism this branch closed
fires only in the control arm, and it is zero-by-construction in the fix arm.
What the last two columns say is that **the family survives the fix** — which
is the point of the *Status* section above, and the A/B makes it sharper rather
than softer.

The control arm reproduced this page's headline symptom outright, with the
whole chain visible in one run:

```
ERROR …gc::guard: in-place old-gen sweep is freeing a block an INTERIOR
  conservative root points into. That root cannot be rewritten, so the next
  allocation reuses and ZEROES the block under it — the ClassId(0) /
  java.lang.Object face.  obj="0x2001d790498" class_id=0 size=4128
…
ERROR …gc::guard: checkcast receiver points into RECLAIMED memory
  obj="0x20027f9f078" location=old-gen FREE BLOCK (reclaimed)
  target_class=java.nio.ByteBuffer
ERROR …gc::guard: …and the old-gen reclamation ring knows what that block held.
  original_class=java/nio/ByteBuffer  freed_by="old-gen mark-compact"
  free_seq=1832115
→ java.lang.ClassCastException: java.lang.Object cannot be cast to java.nio.ByteBuffer
```

That is cause, verdict and user-visible symptom in one arm, and the causal step
is absent from the other — the strongest evidence this page has had that the
interior-root mechanism is real and closed.

And in the same window the FIX arm produced its own occurrence, which is the
more important half:

```
checkcast receiver is an OLD-GEN block this process RECLAIMED while it was
still referenced.
  obj="0x2002ab98b90"  actual_class=java.lang.Integer  target_class=org.h2.mvstore.Chunk
  original_class=org/h2/mvstore/SFChunk  freed_block="0x2002ab98ab8+0x150"
  interior_off=216   interior_root_pointed_in=false
  freed_by="in-place old-gen sweep"
→ java.lang.ClassCastException: java.lang.Integer cannot be cast to org.h2.mvstore.Chunk
```

**Two independent FIX-arm witnesses now agree on the residual's shape**: this
one (`SFChunk`, 336 bytes) and the ~8 KB `Object[]` quoted in *Status*. Both say
`freed_by="in-place old-gen sweep"` and both say
`interior_root_pointed_in=false`. So the remaining gap is:

* on the **in-place** arm, not the compactor (which is default-off since
  2026-08-03 anyway);
* **not** the interior-conservative-root mechanism — these blocks had no
  interior root;
* reaching the reader as the *re-served* face (`actual_class` is a live,
  unrelated object) rather than the all-zero-header face, which is why anything
  gated on `ClassId(0)` stays silent for it. The reclamation ring is what
  answers, because it records what the block held at the moment it was freed.

### Old-gen compaction became default-OFF on 2026-08-03 — read the A/B accordingly

`major_gc` now asks `oldgen_compact_enabled()` (`CRATONVM_OLDGEN_COMPACT`),
which is **off by default** pending root-cause attribution of a separate
corruption compaction was found to cause. That lands after the A/B above was
run, and it changes which arms of this page are reachable in production:

* the control-arm verdict quoted above names `freed_by="old-gen mark-compact"`
  — a path a default-configured build **no longer runs**;
* the FIX-arm residual quoted in *Status* names `freed_by="in-place old-gen
  sweep"` — which is still the live path, and is therefore where the remaining
  gap has to be looked for.

The interior-root fix's compacting half is not thereby moot: it is the
correctness precondition for turning compaction back on, and its regression
test now drives `old_gen_gc(compact = true)` directly rather than through
`major_gc`, so it keeps testing the downgrade rather than silently passing
because nothing compacts.

### The residual is NOT a mark-phase gap — measured at the moment of reclamation (2026-08-03)

This page has spent three sessions asking *which mark source missed the edge*.
That question now has an answer, and the answer is **none of them**.

`CRATONVM_DBG_SWEEP_REFERRERS=1` word-scans, at the moment the in-place old
sweep is about to free, for anything still pointing into the doomed set. It
runs **after** `OldGen::close_live_set`, so the set it scans is the one that
actually gets freed rather than the raw mark result the closure then rescues,
and it CATEGORISES rather than printing the first N — the first sixteen words
of an ascending scan are the sixteen lowest addresses, which was exactly how an
earlier version of this scan reported `1_298_639` "referrer words" of which
every printed one was `doomed → doomed`.

Five in-place sweeps on `TestMVStoreCacheLoop`, `--Xmx 1g`,
`CRATONVM_GC=-moving-young`, JIT on:

```text
doomed=34849   DEFECTS(live_old=0 young=0 root=0) benign(dead_old=901945 unowned=0)
               scanned(old_bytes=536870912 young_bytes=50289344  roots=142261)
doomed=129942  DEFECTS(live_old=0 young=0 root=0) benign(dead_old=855453 unowned=129)
               scanned(old_bytes=536870912 young_bytes=268365920 roots=270692)
doomed=209244  DEFECTS(live_old=0 young=0 root=0) benign(dead_old=966082 unowned=0)
doomed=154356  DEFECTS(live_old=0 young=0 root=0) benign(dead_old=421166 unowned=3)
doomed=70527   DEFECTS(live_old=0 young=0 root=0) benign(dead_old=945249 unowned=37)
```

The three defect columns are the three mark sources that could have missed an
edge:

* `live_old` — a **marked** old-gen object still points at a doomed block. Zero.
  So the mark BFS did not drop an edge it was handed, and `close_live_set` is
  not leaving anything behind either.
* `young` — a word in the young space points at a doomed block, i.e.
  `mark_young_to_old_refs` missed a young→old edge. Zero.
* `root` — a root points straight at a doomed block. Zero.

**Read the `scanned(...)` triple before believing the zeros.** It is there
precisely so this cannot be an inert-lever reading: 512 MB of old-gen backing
store, 50-268 MB of young space, and 142 000-270 000 roots were actually walked,
and the scan demonstrably finds pointers — 0.4-1.0 M of them per sweep, all
`doomed → doomed`, which is the whole-subgraph-condemned-together case the
promotion-seed comment in `sweep_old_gen_non_moving` warns about.

### Root coverage per sweep (2026-08-03)

The follow-on from the referrer scan: if nothing in the heap or the root slice
points at the freed block, was the root slice itself complete? The same summary
line now carries this cycle's cross-thread peer-scan coverage. Five in-place
sweeps:

```text
xt(passes=0 taken_over=0 UNCLASSIFIED=0 xt_roots=0 hw_windows=0 hw_roots=0)   x2
xt(passes=1 taken_over=0 UNCLASSIFIED=0 xt_roots=0 hw_windows=0 hw_roots=0)   x1
xt(passes=1 taken_over=0 UNCLASSIFIED=0 xt_roots=0 hw_windows=3 hw_roots=182) x1
xt(passes=1 taken_over=0 UNCLASSIFIED=0 xt_roots=0 hw_windows=3 hw_roots=189) x1
```

`UNCLASSIFIED` is the one that matters, and it is **zero on all five**. On Linux
a peer is taken over by signalling it and waiting for it to park in the handler;
`STATE_CANCELLED` — no answer within the deadline — means the peer is **still
running JIT code**, and `xt_root_scan`'s own comment says its "JIT-frame oops
are in no root set". A sweep with `UNCLASSIFIED > 0` would have decided liveness
from a root set that provably omitted a running thread's registers and stack —
exactly the root no heap-side scan can see. It has not been observed non-zero.

`passes` exists because without it the rest is unreadable.
`stw_takeover_should_scan` gates round 0 on the cheap `any_thread_in_jit()` hint
and the take-over loop exits as soon as the barrier is satisfied, so a fully
cooperative pause legitimately runs **zero** passes — and then
`taken_over=0 UNCLASSIFIED=0` means "never looked", not "looked and found
nothing". Both readings occur above and they point at opposite conclusions.

`hw_windows` is the post-barrier helper-window pass: a *blocked* peer, excluded
from the barrier, whose native stack still holds JIT frames that its
`deposit_root_snapshot` never covers. It reported **3 windows and ~185
conservative roots on two of the five cycles**, which is what makes the
`hw_windows=0` readings meaningful rather than vacuous — the pass demonstrably
runs and demonstrably reports when there is something to find.

### Where that leaves the residual

On every cycle measured, *both* halves come back clean:

* the heap says nothing live points into the condemned set (`live_old=0
  young=0 root=0`, against 512 MB of old gen, 5-268 MB of young and
  142 000-355 000 roots actually walked, with 0.4-1.0 M `doomed → doomed`
  pointers found so the scan is demonstrably working);
* the collector's own accounting says the root set was complete
  (`UNCLASSIFIED=0`, with the helper-window pass active).

And the family still reproduces. Two possibilities remain, and they are
distinguishable:

1. **The bad cycle has not been caught with the instrument armed.** The events
   are rare (~1 per several worker-hours) and only five sweeps carry the full
   instrument. The instrument is cheap enough to leave on for a long soak; the
   decisive run is one that ends in a `cannot be cast` with these lines above
   it.
2. **The surviving reference is held somewhere neither instrument looks.** The
   remaining candidate is a native identity-keyed side table:
   `native-collections` keeps state in overlays (`clone_lhm_overlay`,
   `properties_sidetable`) and `external_roots` registers owners.
   `CRATONVM_DBG_OLDSWEEP_OWNERS=1` already reports when a freed block **is** an
   overlay owner; nothing reports when a freed block is **referenced by** one,
   and that is the next instrument to write.

## Severity
**HIGH** — silent. Before this session no guard fired: zero
`gen_heap::set_field`/`get_field` out-of-bounds hits, zero
`read_slot: corrupt Value cell` reports, and the first observable symptom was
application-level nonsense on another thread, minutes later.

## What was wrong with the previous diagnosis

The two earlier sessions attributed this to a root-coverage gap in the
**non-moving young sweep**, on the reasoning that the young sweep zeroes every
span it reclaims and the failing receiver has an all-zero header. The inference
step was never checked: `java.lang.Object` is `ClassId(0)`, and so is an
ordinary `new Object()` whose identity hash has not been minted, and so is the
zeroed tail of an old-gen compaction, and so is a freshly-zeroed block the
allocator has not yet stamped a header on. Four different things produce the
same face, and nothing on the failing path told them apart.

It is now measured, not inferred. The receiver lives in an **old-generation
free block**.

## Symptom — the old-gen face

```
java.lang.ClassCastException: java.lang.Object cannot be cast to org.h2.mvstore.Page
    at org/h2/mvstore/FileStore.readPageFromCache(FileStore.java:2089)
```

and — same family, same run shape, caught this session:

```
java.lang.ClassCastException: java.lang.Object cannot be cast to java.nio.ByteBuffer
    at org/h2/mvstore/cache/FilePathCache$FileCache.read(FilePathCache.java:95)
```

Both are `CacheLongKeyLIRS.get()` followed by the cast that generic erasure
puts after it. The same corruption also surfaces as a bare `SIGSEGV` in the VM
(observed `addr=0x5`, i.e. a field read through a zeroed header) with no Java
exception at all — so a run that dies at rc=139 is the same defect, not a
separate one.

## What actually happens

A live **old-generation** object — a cached `Page`, or a `ByteBuffer` held by
`FilePathCache` — is returned to the old-gen free list while the LIRS cache
still references it. `OldGen::free` leaves the bytes in place, so the stale
reference keeps reading the real object until the allocator reuses the block;
reuse zeroes it before writing the new header (`OldGen::allocate` is the sole
zeroing point), and `OldGen::compact` zeroes its whole trailing region. Either
way the next read through the still-live reference sees an all-zero header.

By the time the failure happens the cache chain is entirely old→old:
`FileStore.cache` → `CacheLongKeyLIRS` → `segments[]` → `Segment` →
`entries[]` → `Entry` → `value`. A gap anywhere in `old_gen_gc`'s mark phase
therefore frees the payload under a live parent — and, unlike the young sweep,
the in-place old sweep has no side-mark escape hatch: it frees purely on
`GC_FLAG_MARKED`.

## The blocked-frame face (formerly its own page)

### Its symptom — the face the old page was named after

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
     method="java/lang/Object.hasNext()Z"
     caller="org/h2/test/db/TestMultiThread.testConcurrentUpdate()V @pc=252"

CRATONVM_DBG_CCE_BT: site=nsme_dispatch method=java/lang/Object.hasNext()Z
  CCE-BT-STK[3] org/h2/test/db/TestMultiThread.testConcurrentUpdate pc=252
  CCE-BT-STK[2] org/h2/test/db/TestMultiThread.test pc=28
  CCE-BT-STK[1] org/h2/test/TestBase.testFromMain pc=11
  CCE-BT-STK[0] org/h2/test/db/TestMultiThread.main pc=9
  NSME-RECV addr=0x200625bf088 tid=0 blocked=false epoch=54
  NSME-RECV SHAPE kind=Object num_fields=0 mirror_of=<not a registered mirror>
```

`@pc=252` is the `for (Future<Void> job : jobs)` result loop
(`TestMultiThread.java:381`). The receiver is the synthetic `Iterator` local
javac emits for that loop — held only by `testConcurrentUpdate`'s frame while
the thread is parked inside `job.get(5, TimeUnit.MINUTES)`.

`num_fields=0` and a class of `java/lang/Object` are the ambiguous
`ClassId(0)` face. An `ArrayList$Itr` has fields; this one has none.

A second witness, same loop shape, different method, seen 2026-08-01 on the
pre-`750a95f8e3` tree:

```
NoSuchMethodError method="java/lang/Object.next()Ljava/lang/Object;"
     caller="org/h2/test/db/TestMultiThread.testConcurrentInsert()V @pc=197"
```

HotSpot emits neither, ever.

### 2026-08-03 campaign — what was measured, and what it eliminated

All on `fix/h2-classid0-close-20260803`, `--Xmx 1g`, 16-core Azure host, two
workers, no debug flags beyond `CRATONVM_DBG=cce-bt`.

| phase | binary | runs | family events | other |
| --- | --- | --- | --- | --- |
| 1 | `12769bb23c` (JIT fix, no clone verdict) | 9 | 2 — `CloneNotSupportedException` ×4 in one run, `ClassCastException` ×12 in another | 1 `TimeoutException`, 6 clean |
| 2 | `583021945b` (+ clone verdict) | 18 | 2 — `CloneNotSupportedException` ×12 **with a verdict**, and `ClassCastException` ×4 | 1 `TimeoutException`, 15 clean |

27 runs, 4 family events, ~1 in 7. `rootdead=0` and `audit=0` on every one of
the 27.

`TimeoutException` is the separate throughput defect tracked on
`h2-update-path-throughput-20260802.md`, not this one.

#### Eliminated, with measurements

* **The per-bci live-local mask is not the gap.** The synthetic enhanced-for
  `Iterator` local is read only across the loop's BACK EDGE from after the
  blocking call, so an analysis that did not reach a fixpoint over that edge
  would call it dead exactly where the thread parks — and that mask is what
  `Frame::scan_local_objects` filters the blocked-thread root snapshot with.
  `local_liveness::tests::enhanced_for_iterator_is_live_at_the_blocking_call`
  models the real method's bytecode (loop head 7, `Future.get` at 37, the whole
  range inside a `try` whose handler never reads the slot) and asserts slot 9
  live at pc 37/42/43. It **passes**, and it is differential: slot 10 (`job`)
  is correctly dead at pc 42, so the analysis is doing real work rather than
  returning `ALL_LIVE`.
* **The young sweep is not dropping a published root.** The new
  root-in-dead-span invariant compares `roots` + `finalizer_addrs` — the exact
  set the mark phase was handed — against every span the sweep is about to
  zero, and RETAINS any span a live (non-forwarded) root points into. It
  measured **zero** across the whole campaign (`rootdead=0` on all 18 runs).
  That is an elimination, not a silence: the check runs unconditionally and
  prints when it fires.
* **The blocked-frame slot audit found nothing** (`audit=0` on all 18 runs).
  It checks every live local and stack slot of every frame at blocked-region
  entry and at wake for `class_id == 0 && kind == Object`, and asks the heap
  whether such an address is in a reclaimed hole.

#### Instrumentation added (all unconditional)

* `audit_frames_for_reclaimed_slots`, filtered by the collector's OWN liveness
  mask — a *dead* local pointing into a reclaimed span is the filter working as
  designed — and gated on `class_id == 0 && kind == Object`, because a
  primitive array header also carries class id 0 and without the kind test
  every `long[]` local flags on every wake. Cost is bounded to one frame walk
  per COLLECTION per thread rather than one per blocking call (`f2a19c30af`).
* A lock-free **young-span reclamation ring**, one record per COALESCED span.
  The pre-existing `record_swept` is gated on `CRATONVM_DBG_SWEEP_ZERO`, which
  also swaps the young collector off its parallel sweep prefix — the instrument
  changed the thing it measured, which is this family's entire "reproduces
  plain, never instrumented" history.
* The **clone-face verdict** described in *Status* (`583021945b`).

## Repro

```bash
cd <fresh writable dir>          # H2 writes ./data
CRATONVM_NO_MOVING_YOUNG=1 <cratonvm-bin> \
  --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<probes>:<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.store.TestMVStoreCacheLoop
```

`TestMVStoreCacheLoop` (`apps/h2database-suite-runner/probes/`) is
`TestMVStoreCachePerformance` with the two single-threaded warm-up rounds kept
and the two **10-thread** rounds LOOPED instead of moving on to the 100-thread
ones. The stock class enters the failing regime once, ~730 s in, gives it 4 s
of wall clock, and then leaves it for good. Looping keeps the heap state and
the concurrency identical while multiplying exposure per run.

`CRATONVM_NO_MOVING_YOUNG=1` with the JIT on is still the trick — it holds the
VM on the conservative-JIT-root path, which is what routes old-gen reclamation
through `sweep_old_gen_non_moving` instead of the compactor. Do **not** add
`--nojit`.

**Do NOT set `CRATONVM_DBG_SWEEP_ZERO=1`** — see the next section.

### Observed rate (2026-08-01, 16-core Azure host, `--Xmx 1g`, JIT on)

`TestMVStoreCacheLoop`, six workers, ~50 min per worker-iteration:

| binary | workers | events |
| --- | --- | --- |
| `origin/dev` @ `c8a3ba181d` (first pass) | 7 | 2 — 1 CCE, 1 SIGSEGV |
| `c8a3ba181d` + `5750caf5f` + this branch | 3 | **2 SIGSEGV** |
| `c8a3ba181d` + this branch (no `5750caf5f`) | 3 | **3 — 1 CCE, 2 SIGSEGV** |

Both arms of that A/B ran on the same host in the same window, started
together. `5750caf5f` is therefore **not** the fix.

The stock `TestMVStoreCachePerformance` gave 0 events in 5 full runs (~5.5 h)
over the same period, which is why the loop variant exists.
`org.h2.test.synth.TestDiskFull` — offered by the now-retired
`bug-h2-testdiskfull-classid0-corruption-segv-cce` write-up as a 2-second
reproducer for this family — gave **0 SIGSEGV and 0 CCE in ~110 runs** across
three binaries the same day, so it is not a usable handle right now. See
*Handed over from `TestDiskFull`* below.

### The two faces

```
java.lang.Object      cannot be cast to java.nio.ByteBuffer      (block still free)
java.util.BitSet      cannot be cast to org.h2.mvstore.Chunk     (block re-served)
org.h2.mvstore.Page$Leaf cannot be cast to org.h2.mvstore.Chunk  (block re-served)
```

plus `SIGSEGV` with `slot[rN]` reading eight zero words — the same all-zero
header, reached from compiled code before any reporter runs. A run that dies at
rc=139 is this defect, not a separate one.

Anything gated on the receiver reading as `ClassId(0)` sees only the first
line. Both `checkcast` reporters now consult the old-gen reclamation ring on
*every* failing cast for that reason.

## The instrumentation was part of the problem

`CRATONVM_DBG_SWEEP_ZERO=1` — the flag the previous session added and
recommended so that "the next occurrence self-diagnoses" — feeds
`retain_dead_objects` in `sweep_young_non_moving`, and `retain_dead_objects` is
one of the guards on the **parallel sweep prefix**. Setting it silently swaps
the young collector from the 8-worker anchored parallel walk to the sequential
one and stops dead spans being coalesced. `CRATONVM_DBG_A2`,
`CRATONVM_DBG_SWEEP_CENSUS` and `CRATONVM_DBG_WATCHREF` do the same. The ring
it populates also takes a process-global mutex per reclaimed object.

That is a live candidate for *Session 2*'s central puzzle — ~40 % reproduction
on plain runs on 2026-07-31, then **0/18 the next day on "current dev +
instrumentation"**, concluded as "the variable is the host, not the code". The
instrumented configuration is not the same code.

The replacement needs no flag set in advance.
`GenerationalHeap::reclaimed_hole_at` asks the heap whether the failing
receiver is inside a free-list hole, past the allocation frontier, or in the
inactive semispace; a live `new Object()` is in none of those. It is reached
only from the `checkcast` failure paths, so it costs nothing until something
has already gone wrong. On the reproduction above it printed, with no prior
configuration:

```
ERROR cratonvm::gc::guard: checkcast receiver points into RECLAIMED memory — a
still-referenced object was collected. obj="0x20027fa39f8"
location=old-gen FREE BLOCK (reclaimed) target_class=java.nio.ByteBuffer
```

An unconditional, lock-free **old-gen reclamation ring** now records what each
freed block held and which reclamation freed it (in-place sweep vs
mark-compact), and the same reporter prints it — so the next occurrence names
the mark-phase gap rather than describing its consequences.

## Ruled out, with measurements

Each of these was a live hypothesis; none survived.

* **A young-side marking gap, and a card-table / write-barrier miss.**
  `CRATONVM_DBG_SWEEP_EDGES` walks the whole old generation and the whole young
  from-space after marking and before anything is zeroed, looking for a
  surviving object that references a doomed one. It reports
  `root=0 young-survivor=0 old-gen=0` on every cycle of this workload. (Its
  case (1) had to be fixed first: it tested the RAW conservative candidate
  rather than the base `mark_young` actually marks, so it produced 9–16 bogus
  "mark filter rejected a live root" hits per cycle on a healthy run — enough
  noise to hide a real one. Its young walk also lacked `merge_skips`, so a
  reserved TLAB tail read as "arena already corrupt before this sweep".)
* **The weak-reference path.** `CacheLongKeyLIRS.Entry.getValue()` is
  `value == null ? reference.get() : value`, and `evictBlock()` converts every
  evicted entry to a `WeakReference`, so the failing cast is frequently over
  `Reference.get()` — and `access()` latches the result back into the strong
  `value` field, which explains the doc's original "exactly 4 `cannot be cast`
  lines, one per reader thread that trips over the same corrupted page".
  `WeakRefLirsStress` reproduces that exact shape (a long-lived old-gen entry
  array oscillating between strong and weak under 10 threads) and ran **153 M
  reads / 76 M evict+resurrect cycles with zero corruption**. The clear/restore
  machinery is sound under churn.
* **Off-grid parallel-sweep anchors.** The parallel sweep prefix starts an
  independent chain at each anchor and re-proves it only by landing on the next
  one, which is probabilistic rather than a proof. The sequential walk now
  validates every anchor against the grid it proves:
  `SWEEP_ANCHOR_NOT_A_BASE` measured **zero**.
* **Conservative candidates the anchor oracle cannot place.** A candidate in a
  discarded anchor interval used to be side-marked at its RAW address, which
  retains nothing when the address is object-interior — so the containing
  object would be reclaimed with a conservative root pointing into it. Now
  re-resolved against the arena's own object grid, marked, and drained.
  `LATE_RESOLVE_*` measured **zero** on this workload: a closed hole, not an
  active one.
* **Reserved TLAB tails desyncing the young→old seed walk.**
  `mark_young_to_old_refs` and `fixup_young_old_refs` built their skip list
  from the free list alone, so they parsed the reserved TLAB tails of
  forcibly-stopped in-JIT peers as object headers — while
  `sweep_young_non_moving` skips exactly those regions. That looked decisive:
  the referrer would be YOUNG, which `OldGen::close_live_set` cannot rescue,
  and `CRATONVM_DBG_SWEEP_EDGES` never looks young→old. **Refuted by negative
  control**: a test that plants an unparseable reserved tail between a young
  referrer and its old-gen payload passes with the merge reverted, because the
  desync trips the anomaly path and the conservative word scan still finds
  every old-gen base in the stretch. The merge landed anyway as a consistency
  fix, and is labelled as one.
* **Cross-thread root coverage** — refuted in *Session 2*; nothing here
  re-opens it.

## Young-sweep invariants added along the way

Not the cause here, but they were missing and they are the same class of
silent failure, so they are now checked unconditionally (one merge pass each
over lists that are already built and already sorted):

1. **No live object inside a span the sweep is about to zero.** `side_sorted`
   is the complete live set and `dead_regions` is every condemned span; they
   must be disjoint, and can only overlap if the walk left the object grid.
   A failing span is RETAINED rather than freed — a bounded leak and a loud
   report instead of a silent use-after-free.
2. **A span published to the free list is not already on it.** A double free
   lets the allocator serve the same bytes twice and zero them under the first
   owner. Previously only a `CRATONVM_DBG_A2` diagnostic, and an
   O(dead × free) double loop.

## What to try next

In rough order of what the evidence supports.

1. **Enlarge the old-gen reclamation ring, then reproduce.** The ring is 16 K
   entries; an in-place old sweep frees thousands of blocks per cycle and the
   failing read can be many cycles later, so it wraps before the `checkcast`
   asks. The one reproduction that reached the widened reporter
   (`Page$Leaf cannot be cast to Chunk`) got no ring hit for exactly that
   reason. Sizing it in the hundreds of thousands would have named the victim
   class and the freeing site outright — that is one build away and it is the
   highest-value next step.
2. **Add the young-side equivalent.** The ring only covers old-gen frees. The
   young sweep's span zeroing is recorded by `zero_forensics`, but that is
   flag-gated and mutex-backed. A lock-free, unconditional span ring (one
   record per COALESCED span, not per object, so the cost stays bounded) would
   make the reporter answer for both generations.
3. **Instrument the free, not the read.** Everything above still diagnoses one
   GC cycle too late. The decisive check is at the moment of reclamation: for
   each block the in-place sweep is about to free, does any live YOUNG object
   reference it? That is the young→old mirror of `close_live_set` and nothing
   currently performs it — `close_live_set` closes over old-gen referrers only,
   and `CRATONVM_DBG_SWEEP_EDGES` only ever looks for references INTO young.
   Expensive as an always-on check; viable behind a gate for a repro run, and
   it would name the exact cycle and referrer.
4. **Do not re-derive the closed hypotheses.** The four in *Ruled out* each
   cost a build-and-soak cycle; they are recorded with their measurements so
   the next session does not pay for them again.


### From the blocked-frame face

5. **Find where the reference goes wrong, not where it is read.** The clone
   verdict says the receiver is a live `java.lang.Thread` at an address the
   caller's `long[]` reference should never hold. Work backwards from
   `BitSetHelper.flip` -> `Arrays.copyOf(long[], int)`: which load produced it?
   `CRATONVM_JIT_BISECT_ONLY` narrowed the sibling JIT defect to two classes in
   about ten runs and is the tool for this too.
6. **Re-take anything measured before `12769bb23c`.** A wrong-object return
   from a virtual call is not distinguishable, at the reader end, from a stale
   reference, and that defect was live for this family's whole history.
7. **Count all four faces**, not one. See *The four faces* above.
8. **Do not re-derive the eliminations.** Each cost a build-and-soak cycle and
   each is recorded with its measurement.

## Handed over from `TestDiskFull` (2026-08-01)

The sibling report `bug-h2-testdiskfull-classid0-corruption-segv-cce.md` was
resolved and retired the same day by a concurrent line of work, and it hands
this doc one residual. Both halves of that write-up corroborate the diagnosis
above:

* Its symptom 1 — the `class_id=ClassId(0) num_slots=0` guard bursts — was
  fixed by `c0d09e2451` and `b86945eafe`, *"post-GC reference processing wrote
  through stale **old-gen** addresses"* and *"the refproc staleness guard had
  the same old-gen blind spot"*. Same generation, same shape, a fix that had
  already landed. Its constant `0xe0` receiver-to-value delta turned out to be
  `Reference`/referent pair spacing, not a miscomputed field offset.
* Its residual is **one `SIGSEGV` in 330 runs on `dev@c8a3ba181d`** — the exact
  binary this doc's reproduction ran on — with `r10` pointing at eight zero
  words, i.e. an all-zero object header inside compiled code. Not root-caused
  there because it did not recur; 203 further runs *armed with
  `CRATONVM_DBG_SWEEP_ZERO=1`* produced no second occurrence and no ring hit,
  which is the instrumentation confound described above showing up a third
  time.

`TestDiskFull` is therefore in the same family but is **not** currently a
cheaper handle on it: 110 short-form runs here across three binaries produced
0 `SIGSEGV` and 0 `ClassCastException`. `TestMVStoreCacheLoop` is.

## Related

* `../../internal/fixed-suite-bugs/jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`
  — the JIT miscompile that made this family's reproduction impossible, and the
  reason a wrong-object return has to be excluded before a stale reference is
  assumed. **Read this before attributing anything here to GC.**
* `../../../gc/old-sweep-liveness.md` §7 — the interior-conservative-root fix,
  its counters and its negative control.
* `../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`
  — array receivers dispatched through their COMPONENT class id, the *other*
  defect that puts a receiver into `java.lang.Thread.clone`. The clone-face
  reporter added here exists to tell the two apart.
* `h2-update-path-throughput-20260802.md` — the class the
  blocked-frame face was found in, whose own problem is throughput, not this.
* the retired `bug-h2-testdiskfull-classid0-corruption-segv-cce` write-up —
  same signature; see *Handed over from `TestDiskFull`* above.
* `c0d09e2451`, `b86945eafe` — post-GC reference processing writing through
  stale OLD-GEN addresses, and the same blind spot in its staleness guard.
* `5750caf5f` — *close the live set before the in-place old sweep decides what
  is dead*; its counters are the ones to watch for a recurrence.
* `7303483521` — the old-gen **fragmentation** consequence of the same regime.
  Unrelated cause, unrelated fix.

### Superseded page names

Both former filenames asserted a mechanism the measurements retracted, so
neither name should be resurrected:

* `bug-h2-blocked-frame-classid0-dispatch-miss.md` — "dispatch miss" and the
  blocked-frame framing survive only as *one face*; its liveness-mask
  hypothesis is eliminated with a differential test, and its one verdict says
  the receiver was never reclaimed at all.
* `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md` — "non-moving
  sweep" was retracted by that page's own first paragraph; the generation is
  old gen and the arm is the in-place sweep.

## A different producer of the same face, found and FIXED (2026-08-02)

**Not this doc's bug** — this one's victim is a young object, and the memory
here is measured to be an old-gen free block. But it produces the identical
`ClassId(0)` / all-zero-header face, it was live on `dev` the whole time, and it
is now fixed, so rule it out before attributing a future occurrence.

### The defect

`sweep_young_non_moving`'s selective promotion pins by raw slot **value**:

```rust
for r in roots.iter() {
    let a = r.as_ptr() as usize;
    if is_y(a) && !(honour_movable && is_movable_jit_root(a)) {
        pinned.insert(a);        // <- the ADDRESS THE ROOT HOLDS
    }
}
…
if marked && aged && !pinned.contains(&addr) { /* evacuate to old gen */ }
                                    // ^ `addr` is the OBJECT BASE
```

and its safety comment states the invariant it believed it had: *"selective
promotion is safe under exactly that regime because it pins by raw slot VALUE (a
conservative false positive pins a random object; it can never mis-relocate
one)."*

That holds only while the root **is** an object base. A conservative root is
frequently an **interior** word — a field address, a derived pointer, a spilled
register mid-object — and `mark_young` keeps the object alive by resolving that
word to its containing base through the exact-base oracle. So for an interior
root the two sets disagree: the base is retained, `pinned` does not contain it,
and selective promotion evacuates it to old gen. The raw stack/register word
that kept it alive cannot be rewritten — that is the entire reason this
collector path exists — so it goes on pointing at the young source, which the
sweep frees and `zero_spans_parallel` zeroes. The next dereference through that
root reads `class_id=ClassId(0) num_slots=0`.

### Reproducer — 9 seconds, single-threaded

```bash
CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_DBG_SWEEP_LIVENESS=1 \
  <cratonvm-bin> --java-home /home/victor/jdk25 --Xmx 1g \
  -c <bench-classes> BinTreesClassic 18
```

| build | runs | sweeps | runs with a root pointing into a doomed span |
| --- | --- | --- | --- |
| before | 16 | ~190 | **13** |
| before | 20 | ~240 | **16** |
| before | 36 | 432 | **14** |
| after | 24 | 288 | **0** |
| after (on the merged tree) | 20 | 240 | **0** |

Every pre-fix hit read the same way, which is what named the mechanism:

```
[SWEEP-LIVENESS root] victim=0x2004ec0bfe0 covered=0x2004ec0bfd0..0x2004ec0c010
    in_verified_span=true side_marked_base=true side_marked_raw=false
    header_marked=false forwarded=true fwd=0x20010000670 was_candidate=true
    dead_obj=0x2004ec0bfd0 … interior_off=16
```

`side_marked_base=true` (the mark phase retained it) + `forwarded=true` (promotion
moved it anyway) + a non-zero `interior_off` (the root is not the base).

### The fix

Resolve each pinned root through the same oracle `mark_young` uses and pin the
resolved base as well. Pure over-retention: the object stays in young one more
cycle. `bench/BinTreesClassic` checksums unchanged at d=10/14/16/18 (135854,
3222190, 14985902, **68332206** — the HotSpot-verified value) in both the default
and forced-non-moving configurations, and an interleaved 8×2 A/B on bt18 shows no
throughput cost (pre 2485 ms mean, post 2454 ms).

Regression test: `gen_heap::tests::interior_conservative_root_pins_the_object_it_points_into`
— verified to FAIL without the fix, with the `ClassId(0)` face itself
(`the young source must still be a live object, not a zeroed span`).

### Why nothing here caught it before

`CRATONVM_DBG_SWEEP_EDGES`'s `is_unmarked_young` returns **false** for anything
in the side-mark set, and these victims are side-marked — so a
`root=0 young-survivor=0 old-gen=0` report is silent about this family by
construction. The `CRATONVM_DBG_SWEEP_LIVENESS` assertion only looked at heap
edges (old→young, young-survivor→young), never at the root set, and its
young→young half is vacuous whenever selective promotion moved every survivor
out (measured: `young_survivors_scanned=0` on every BinTrees cycle). Both gaps
are now closed, and the assertion prints a per-sweep line even when it finds
nothing, so a clean campaign is positive evidence rather than silence.

## The blocked-thread face, and the in-cycle check for it (2026-08-04)

A Hibernate witness of this family (`sql.exec.SmokeTests#testQueryConcurrency`,
one occurrence) was filed as a separate page and has been retired into
`../../internal/fixed-suite-bugs/hibernate/smoketests-stale-pointer-nosuchmethod-crash-20260804-RETIRED.md`.
Two things from it belong here, because they apply to any future occurrence:

**`reclaimed_hole_at`'s verdict already names the collector — read it before
attributing anything.** `location=young TO-space (the inactive semispace)` is the MOVING
collector's face — the arena a Cheney cycle evacuates, swaps out and zeroes —
and it is NOT the non-moving sweep's `young from-space FREE BLOCK (reclaimed)`.
An address there is a *pre-copy* address that something failed to remap or
failed to evacuate. Cross-check it against `young_freed_lookup`, which is
unconditional: a non-moving-sweep victim is always in that ring, and this face
never is. The Hibernate page was attributed to the DoHead Layer-1
register-invisible-root mechanism on the strength of the `ClassId(0)` header
alone, and both of those checks refute it.

**The invariant is now checked where it breaks, for blocked threads.**
`ThreadRegistry::fold_pointer_map_into_blocked` asserts, unconditionally and at
the end of every moving cycle, that no `slot_origins` entry — the exact
`(frame, slot)` tracker for a thread inside a blocked region — points into the
semispace that cycle just evacuated. A hit names tid / frame / slot and carries
`was_a_scanned_root`, which forks the fix: `false` = the blocking deposit never
published the slot (root COVERAGE gap), `true` = the collector was handed it and
did not evacuate it (EVACUATION gap). Under `CRATONVM_DBG_BLOCKGC` the deposit
side additionally reports `UNPUBLISHED-LIVE-SLOT` — a live object frame slot the
snapshot omitted — which is the same question asked with no GC race required.

So for a blocked-thread occurrence, the verdict now arrives from the collection
that caused it rather than from whichever bytecode dereferenced it later. If a
fresh `ClassId(0)` report has neither line above it, the holder was not in a
blocked region and the gap is elsewhere.

One root-coverage defect on that path was fixed at the same time, and it is not
specific to blocked threads: all FOUR copies of the frame root scan
(`collect_roots`, `update_root_snapshot`/`scan_frame_roots`, the blocking
deposit, and the frozen-in-JIT peer scan) re-screened their operand-stack roots
with the strict `is_object_address` header probe. That probe drops young /
mid-init objects and the `is_heap_addr`-validated JNI long-as-jobject roots,
re-introducing two already-fixed regressions on the operand-stack half only,
while the locals half of the same loop used the loose screen. All four now use
`is_heap_addr`; see
`root_snapshot_screen_tests::operand_stack_roots_use_the_same_screen_as_locals`,
which fails on the pre-fix screen with `1 of 2 survived`.
