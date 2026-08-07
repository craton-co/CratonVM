# The `ClassId(0)` family — a reference that no longer names what its holder thinks

> **Consolidated 2026-08-03** from two pages that were tracking one defect:
> `bug-h2-blocked-frame-classid0-dispatch-miss.md` (the blocked-frame /
> `Object.hasNext()` face) and
> `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md` (the old-gen
> face). Both old filenames asserted a mechanism their own contents had already
> retracted — the second says so in its first paragraph — which is why the new
> name states only what is actually measured.

## Status

**REOPENED 2026-08-07** — this page's own closing instruction is *"if a
`ClassId(0)` receiver reappears, reopen this page rather than starting a new
one, and check `object_degradation_count()` first."* It has reappeared, on a
binary that contains the 2026-08-06 fix (`git merge-base --is-ancestor
b50da356e f9315411a` → yes), through the RE-SERVED face this page already
describes rather than the all-zero one. **`object_degradations = 0` on the
failing run** — so it is not the `CompactValue` provenance mechanism recurring.
Four witnesses, a reproducer at ~4 events in 46 runs, and the one measurement
this page has never had — the state of the reference *before* the read barrier
touched it — are in
**[REOPENED 2026-08-07: the barrier rewrites a live reference](#reopened-2026-08-07-the-barrier-rewrites-a-live-reference)**
at the end. Everything between here and there is the 2026-08-06 writeup,
unchanged; that fix is real and its validation stands.

**FIXED 2026-08-06** — `fix/compactvalue-object-provenance-20260806`, merged to
`dev` as `b50da356e`.

**Root cause: `CompactValue`'s SUB_OBJECT encoders never recorded into the
provenance bitmap its own decoders consult**, so the encoder could mint a
reference slot the decoder refused. The refusal degrades the value to
`Value::Long`, which takes `LKIND_LONG`, and `Frame::scan_local_objects` skips
LONG slots by design — so the root is never published and the sweep reclaims a
live object. The full derivation, the four encoders, and the measurement that
named it are in **[ROOT CAUSE (2026-08-06)](#root-cause-2026-08-06-a-sub_object-slot-the-encoder-minted-and-the-decoder-refused)**
below. Everything above and below that section is the hunt as it was recorded,
left intact because several of its measurements are load-bearing negatives.

**Validation.** `TestMultiThread` x20 on the fixed binary: **zero degradations
and zero `"result" is null`**, against a baseline of 6-in-32 on the immediately
preceding binary (p ~ 0.016 on its own). Ten clean passes; the other ten were
`TimeoutException` on a host at load 20-90 with 8000 logged-in users, every one
degradation-free, and the passing runs were *faster* than baseline (456-947 s
vs 689-1249 s), so the added recording on the GC relocation path costs nothing
measurable. Gates on the merged tree: `cratonvm-types` 493/0, `cratonvm-gc`
974/0, `cratonvm-vm` 2420/0.

**Residual, stated honestly.** Only one of this page's faces (`"result" is
null`) was frequent enough to measure directly. The `ClassId(0)` dispatch-miss
face and the `The database has been closed` face did not occur in the 32-run
baseline either, so 0-in-20 does not by itself retire them — what retires them
is that the mechanism explains every recorded witness, including the ones this
page could never reconcile (`in_published_snapshot=false` on a RUNNING mutator
whose top frame holds the address, with `ROOT_IN_DEAD_SPANS=0` and
`SWEEP_LIVENESS hits=0`). If a `ClassId(0)` receiver reappears in
`TestMultiThread`, reopen this page rather than starting a new one, and check
`object_degradation_count()` first.

**Earlier fixes that landed under this page and remain valid.** An old-gen mark
gap (`old_gen_gc`'s root seed had no resolution for an INTERIOR conservative
root), and separately a JIT miscompile that bound an `invokevirtual` to the
compiled entry of its CONSTANT-POOL-resolved method with no receiver guard
(`../jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`). The second is
not a GC bug at all, and it mattered here twice over: it made this family's only
reproduction fail 100% of runs in 2-9 s so nothing could be measured, and a
wrong-object return is **indistinguishable at the reader** from a stale
reference — so it stays a live alternative explanation for every occurrence
recorded before `12769bb23c`.

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

### A fifth reproduction: Hibernate `SmokeTests#testQueryConcurrency` (2026-08-05)

Added because this class is a **cheap, JIT-only reproduction** — ~15 s per
attempt at `-Dcraton.smoke.forks=1` — and because it was until now filed under a
throughput page that explicitly told triagers not to look for a crash here. That
page is retired
(`fixed-suite-bugs/hibernate/smoketests-concurrent-query-throughput-20260723-RETIRED.md`);
the throughput finding it was tracking is closed, and this is what is left.

Twenty-six runs at `forks=1`, dev tip, JIT on, in two batches (10 then 16),
produced **two distinct non-completions** — one of each shape:

* `EXCEPTION_ACCESS_VIOLATION`, read of `0x0000000000000005`, in a compiled
  frame under `PooledConnections.poll` /
  `DriverManagerConnectionProvider.getConnection`; `rdi=0xFFFFFFFFFFFFFFFF` —
  the KINDOF sentinel. **Zero GC cycles had run** (`0 moving cycle(s), 0
  diverted`), and the report names
  `unregistered-jit-frame-on-stack` as the last incomplete-coverage reason.
* a SessionFactory that failed to build at all —
  `PropertyNotFoundException: Could not locate getter method for property 'id'`
  — preceded by a burst of
  `gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)`. The
  payloads are String byte data read as a `Value` cell
  (`raw0="0x006c6d782d6d6268"` is `"hbm-xml"`,
  `raw0="0x0065646163736163"` is `"cascade"`).

Eight `--nojit` runs of the same command were clean, 8 for 8, so the JIT is
required. The zero-collection detail matters for this page's own open question:
like the clone face, and unlike the old-gen face, there is **no evidence any
collection freed anything** here — a wrong-object return or a stale VM-side
cache entry remains as live an explanation as reclamation.

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

### The same scan on the COMPACTING arm, and the trap in it (2026-08-05)

The section above measures the **in-place sweep**. Repeating it against the
**compactor** — at `--Xmx 512m`, which reaches the compacting regime an order of
magnitude faster (9 major GCs in 25 minutes against 1 in 90 at `1g`) — gives the
same verdict, but only after a false positive is removed from the instrument.

Unvalidated, `live_old` fired on **every** compaction: about one hit each,
always the same shape, byte-identical across three independent processes.

```text
REFERRER class_id=64 num_slots=18 total_size=320 compact=false
  slot_of_word=88   enumerator_yielded=2   ENUMERATOR_SEES_VICTIM=false
CELL 5 raw=[0x0000000200000000, 0x00000200100c4020] decodes_as=Discriminant(0)
```

Discriminant 0 of `Value` is `Int`. The cell reads `Int(2)` — tag and payload in
its low half — while the victim's pointer sits in the 16-byte cell's **unused
upper half**: residue from an `Object` that previously occupied the slot and was
overwritten by a narrower variant. The mutator can never read it, the GC
correctly ignores it, and the victim really is dead.

**A raw word scan of this heap lies, and it lies in the direction of
manufacturing a defect.** `live_old` now asks the containing object's own slot
enumerator — the one every mark source funnels through — whether it actually
yields the victim, and counts the rest as `stale_padding`. With that in place,
over 12 compactions across 3 processes and ~2.4 M dropped blocks, `live_old`,
`young` and `root` are **0** throughout.

The earlier "`1_298_639` referrer words, every printed one `doomed → doomed`"
nuisance and this one are the same problem seen twice. The fix for the first was
to categorise instead of printing; the fix for the second is to decode instead
of comparing bytes. **Both are needed**, and neither subsumes the other.

`Value`-cell residue is also a *proven* mechanism for stale copies of an address
existing in the heap long after the field stopped holding one — the cheapest
known source of "an address that no longer names what its holder thinks it
names", which is what this page is called. What is still not shown is a path by
which such a copy is ever *read*.

### The decisive run: `UNCLASSIFIED` is NOT always zero (2026-08-05)

> **Superseded the same day — see *The dose-response that falsified it* below.**
> The measurement in this section is real; the inference drawn from it is not.
> Unclassified peers turn out to be neither necessary nor sufficient for the
> face.

*Where that leaves the residual* below asks for exactly one thing: "the decisive
run is one that ends in a `cannot be cast` with these lines above it." Here it
is. `TestMVStoreCacheLoop`, `--Xmx 512m`, `CRATONVM_GC=-moving-young`, JIT on,
referrer scan armed, run ended in 4 `ClassCastException`s:

```text
[GC] generational: minor=428 major=5
[GC] oldgen_compact: dropped_watched_referents=312202 dropped_interior_root=1
                     downgraded_to_inplace=0
[GC] decision histogram: moving=270 non_moving=158
[GC] xt_peer_scan: unclassified_peers=16 cycles_with_unclassified=15
```

and all four compactions in that same run reported
`LIVE_REFERRERS=0 young=0 root=0`.

**`UNCLASSIFIED` is 16, over 15 cycles.** The 2026-08-03 campaign observed it
zero on all five sweeps it measured and concluded the root set was complete;
that conclusion does not hold for the runs that actually fail. Per
`xt_root_scan`'s own comment, an unclassified peer is one that did not park in
the signal handler within the deadline — i.e. **still running JIT code, whose
frame oops are in no root set**. That is precisely a reference no heap-side scan
can ever see, and it is consistent with every other measurement on this page:
nothing in the heap names the block, nothing in the root slice names it, and the
collector nevertheless had an incomplete root set on 15 cycles of the failing
run.

This does not prove the specific victim was named by an unclassified peer — that
needs the per-cycle `xt(...)` line beside the reclamation that drops it. It does
retire the premise that the root set was complete, which is what possibility (1)
below was waiting on, and it makes cross-thread root coverage the live suspect
again rather than a hypothesis "refuted in session 2".

The same run also shows `dropped_interior_root=1`: the compaction dropped a
block an INTERIOR conservative root pointed into. That run carried
`CRATONVM_GC_NO_OLD_INTERIOR_PINS=1` (the pin disabled, as a control), so this
is a direct observation of the defect the interior-root fix removes, firing in a
run that then produced the family's symptom. It is one contributor, not the
whole family — `LIVE_REFERRERS=0` says the rest of the dropped set was
genuinely unreferenced.

### The dose-response that falsified it (2026-08-05)

The section above made cross-thread coverage the prime suspect off a single
correlation. `CRATONVM_XT_PEER_DEADLINE_MS` makes that testable without a
rebuild: it is the deadline a signalled peer has to reach the take-over handler,
and missing it is what produces `STATE_CANCELLED` — the unclassified reading.
Three arms, same binary, same class (`TestMultiThread`), same `--Xmx 1g`, same
host, one variable:

| arm | deadline | unclassified peers | faces |
|-----|----------|--------------------|-------|
| XD1 | 1 ms     | **45**, 20 cycles  | **0** |
| HH1 | 20 ms (default) | 0             | 0     |
| XD3 | 3000 ms  | **0** (whole run)  | **1** |

The arm with forty-five unclassified peers produced no failure; the arm with
none produced one. **Unclassified peers are neither necessary nor sufficient for
this face.** Do not restore that hypothesis without new evidence.

#### The silence that produced the wrong reading

`vm-cli/src/main.rs` printed the `xt_peer_scan:` summary only `if peers > 0`. So
a run that *failed* printed nothing — and nothing looks like the reassuring
answer while actually covering three others: the scan ran and found nothing, the
scan never ran, the feature is off. `gc_quiescence::XT_PASSES_LAST_CYCLE`
documents this exact trap one level down ("`taken_over=0 unclassified=0` means
'never looked', not 'looked and found nothing' … they must not share an
encoding"), and the process summary simply did not follow it. It is now
unconditional and carries `taken_over=`, `helper_windows=` and `enabled=`.

### The holder verdict, and what it retires (2026-08-05)

The page has been asking which object still holds the stale reference. The
answer, from `live_holders_of` on the unconditional guard path — a walk of old
gen plus the live young arena, decoding through each object's own
`for_each_ref_slot` rather than comparing raw words:

```text
gc::guard: receiver points into RECLAIMED memory …
     obj="0x200498491e8" site="invoke dispatch"
     location=young TO-space (the inactive semispace) span="0x20042400000+0x0"
gc::guard: …and NO live heap object holds this address in a decoded reference
     slot. The holder is therefore a frame local, a register, or a native side
     table — not a heap field.
gc::guard: receiver is inside a YOUNG span the non-moving sweep zeroed and
     returned to the free list … sweep_cycle=0 free_seq=2420 interior_off=26320
```

Two things follow, and both correct earlier entries on this page.

**The `TO-space` label is an artifact, not a finding.** `span=…+0x0` — the
extent is zero. Young collections in these runs are essentially never moving
(`decision histogram: moving=1 non_moving=38`), so the inactive semispace is
empty and an address "in" it is really in the region the non-moving sweep freed,
which the very next line says outright. Every reading built on that label — in
particular "the reference survived un-rewritten, it was not unrooted", i.e. a
*remap* gap — rests on a label the collector prints regardless. The authoritative
line is the sweep one, and this is a **mark** gap.

**Heap-side referrer scanning was never going to answer.** The holder is a stack
slot, a register, or a native side table. That retires `LIVE_REFERRERS=0` as
evidence of anything: it asks whether a *heap object* points at a doomed block,
and the answer here is that none ever did.

#### The asymmetry that lets this happen

Every coverage-incompleteness signal in this collector downgrades **relocation**
— `mark_moving_young_coverage_incomplete_because` and the `[moving-young]
fallback #N` warnings, whose reasons in the failing run are
`unregistered-jit-frame-on-stack`, `innermost-rbp-belongs-to-unguarded-callee`
and `compiled-frame-oop-not-published`. **None of them downgrades
reclamation.** `xt_cycle_coverage()` has exactly one caller in the tree: a
`CRATONVM_DBG_SWEEP_REFERRERS`-gated printer. So the collector can know its root
set omitted a running thread's frames and still free on `GC_FLAG_MARKED` alone —
and the non-moving sweep, which incompleteness *selects*, is the path that
frees.

The guard now reports the coverage of the sweep that freed the specific span,
captured while that sweep ran: `root_coverage=complete` / `INCOMPLETE` /
`NEVER-LOOKED`, with `xt_passes` / `xt_taken_over` / `xt_unclassified` beside it.
`NEVER-LOOKED` is a distinct reading on purpose — the take-over is gated on an
`any_thread_in_jit()` hint, so zero passes means the scan never looked, which is
the opposite conclusion from zero unclassified peers.

### The take-over contributes nothing here (2026-08-05)

With the summary made unconditional, every arm reports the same thing:

```text
[GC] xt_peer_scan: unclassified_peers=0 cycles_with_unclassified=0
     taken_over=0 xt_roots=0 helper_windows=253 resignals=16
     classified_after_retry=0 enabled=true
```

**`taken_over=0` and `xt_roots=0` on 6 of 7 runs measured.** The seventh
reported `taken_over=1 xt_roots=1298`, so the take-over is not dead — it is
rare, and when it does fire it contributes a lot. The routine case is that it
freezes nobody: cross-thread coverage in this workload comes almost entirely
from the helper-window pass (100-570 windows per run), which handles peers
blocked in native code with JIT frames below them.

That is consistent with the dose-response above — a deadline on a pass that
usually classifies nobody has little to act on — and it retires the take-over
deadline as a factor here. It does NOT say the take-over is useless; a single
pass contributing 1298 conservative roots is the opposite of useless, and that
run is a reminder to keep the counter rather than the impression.

It also confirms what the retry does: `resignals=16` with `unclassified=0`,
against **16** unclassified peers on the pre-retry twin run. Those sixteen were
converted to definite `STATE_NOT_JIT` answers. (The `classified_after_retry`
counter read 0 while doing so — it only incremented in the `STATE_PARKED` arm —
and has been corrected to count every definitive answer that needed a
re-signal.)

### The unregistered-JIT-frame memo is unsound across a re-descend (2026-08-05)

Found by reading, then given both a fix and a direct measurement.

`scan_active_jit_frames` detects a JIT frame that is live WITHOUT having pushed
an entry guard — the A5 case — by looking for a JIT return address in the band
above the registered chain. On a hit it conservatively marks that band and flags
the cycle non-moving. Miss it and that frame's oops are never marked, so the
non-moving sweep frees them while it is live: this family's exact face.

The detection is memoized by `UNREG_JIT_VERIFIED_LO`, justified as:

> nothing above our current stack pointer can change while we are nested below it

That statement is true, and it does not support the memo, because **the memo
outlives the nesting**. `verified_lo` only ever moves deeper. A thread that
returns above it, enters an already-compiled method — which pushes no guard and
adds no code range — and descends again will short-circuit the detection over a
band that was rewritten in between, for the rest of its life. The memo's only
staleness guard is `jit_code_range_count()`, which changes when a method
COMPILES, not when one is ENTERED. The sibling cache in the same file
(`JIT_SCAN_CACHE`) keys on `JIT_BOUNDARY_GEN` for the analogous reason; this one
never did.

It applies to peers as well as to the collecting thread: the same memo gates the
per-native-call `update_root_snapshot`, so a parked peer's *published* snapshot
inherits the stale verdict.

**Fix.** Track the shallowest stack pointer observed since verification. Rising
to `hiwater` pops every frame below it and leaves everything at or above it
untouched, so the still-clean floor is `max(verified_lo, hiwater)` and the band
between is rescanned. A thread that only descends — the perpetually-deepening
recursion the incremental path was written for — sees `hiwater == verified_lo`
and behaves exactly as before; a regression test asserts that, and another
asserts the returning-thread sequence the old rule got wrong (it also pins the
old rule's wrong answer, so the fix cannot be silently reverted).
`CRATONVM_JIT_UNREG_MEMO_HIWATER=0` restores the old rule for A/B.

**Measurement.** `CRATONVM_DBG_UNREG_MEMO_AUDIT=1` runs the detection scan even
when the memo says clean and counts the disagreement, reported unconditionally
as `[GC] unreg_memo: shortcircuits=N SUPPRESSED=M`. It marks nothing and changes
no collector decision, so unlike most instruments on this page it cannot perturb
what it measures. `SUPPRESSED > 0` means the memo hid a real unregistered JIT
frame. The denominator is printed beside it for the reason this page has now
learned twice: a zero with no denominator is not a measurement.

### The memo suppresses real detections, measured (2026-08-05)

`CRATONVM_DBG_UNREG_MEMO_AUDIT=1`, one `TestMultiThread` run at `--Xmx 1g`:

```text
[GC] unreg_memo: shortcircuits=222869 SUPPRESSED=972
```

The memo answered "no unregistered JIT frame above here" 222,869 times without
scanning, and in **972** of those a scan of the same range found one. Each is a
detection that did not happen — so that frame's oops were not conservatively
marked and the cycle was not forced off the moving path, which is the mechanism
that leaves a live object unmarked for the non-moving sweep to free.

The denominator is the point of the line. `SUPPRESSED=0` beside
`shortcircuits=0` means the audit never ran; beside `shortcircuits=222869` it
would mean the memo is honest. Those must not look alike — see the
`if peers > 0` mistake above, which is the same error one instrument earlier.

**Measured with the hi-water rule already ON.** So that rule is a real but
PARTIAL improvement: it can only react to stack-pointer rises it happens to
observe, and a thread that returns above the verified point and re-descends
entirely between two root-snapshot calls never presents one. It is not the fix
and is not claimed as one.

#### The hi-water A/B, and what it does not show

Same binary, same class, same heap, arms run CONCURRENTLY so host load (which
ranged 20-124 during this session) hits both:

| arm | hi-water | short-circuits | SUPPRESSED |
|-----|----------|----------------|------------|
| AU  | ON       | 222 869        | 972        |
| AU  | ON       | 228 108        | 431        |
| AU  | ON       | 277 271        | 754        |
| AU2 | OFF      | 276 145        | 790        |

**No measurable effect on either column.** The ON runs straddle the OFF run on
both metrics and their own spread (431-972 suppressed, 223 K-277 K
short-circuits) is wider than any gap to the control.

An intermediate reading of the first two ON samples looked like a ~19% reduction
in short-circuits; the third sample (277 271, with the rule ON) removed it. Two
points against one is not a measurement on a host whose load ranged 20-124
during this session — see the standing note on interleaved A/B here.

The rule is kept anyway, described for what it is: a sound tightening of an
argument that was plainly wrong as written (`verified_lo` alone claims the
verdict holds for the life of the thread), with no demonstrated effect on this
workload. It is **not** credited with fixing anything, and the correctness
argument rests entirely on the authoritative reset below.

### Two caches, one invalidation hook (2026-08-05) — the load-bearing fix

`conservative_roots.rs` memoizes two different per-native-call scans:

| cache | what it decides | keyed on |
|-------|-----------------|----------|
| `JIT_SCAN_CACHE` | the conservative root set | `JIT_BOUNDARY_GEN` |
| the unregistered-frame memo | whether an unguarded live JIT frame exists above the chain | `jit_code_range_count()` only |

`invalidate_scan_cache_for_gc()` exists because such a cache can be stale by GC
time. Its own doc states the rule — *"we only need the **authoritative** GC root
scans to be fresh"* — and it is called from all four authoritative sites:
`collect_roots`, the safepoint publish, the pre-park publish, and the
blocked-path deposit. It bumps the boundary generation, which discards the first
cache and **does nothing to the second**, because the second is keyed on a
question the hook does not answer.

So the identical soundness gap was diagnosed and closed for one cache and left
open on the other, forty lines away — and the one left open is the more
dangerous: a stale root snapshot drops individual references, while a stale
unregistered-frame verdict drops an entire frame's worth AND leaves the
collector believing it may relocate.

**Fix:** reset the memo in `invalidate_scan_cache_for_gc` too. Suppression on
the paths a collector actually consumes becomes impossible by construction,
because every one of them invalidates first. The cost is one band rescan per GC
root collection instead of per native call — exactly the trade that function
already documents, and the reason the memo can stay for its hot purpose.
`a_reset_memo_demands_a_full_rescan_at_any_depth` pins the property that a reset
memo asks for a FULL rescan rather than an incremental band over a prefix
nothing has verified.

### The face that read the new fields (2026-08-06)

`TestMultiThread`, `--Xmx 1g`, on the arm running the PRE-fix behaviour
(`CRATONVM_JIT_UNREG_MEMO_GC_RESET=0 CRATONVM_JIT_UNREG_MEMO_HIWATER=0`):

```text
gc::guard: receiver points into RECLAIMED memory obj="0x2004a5f3ab8"
     site="invoke dispatch" location=young from-space FREE BLOCK (reclaimed)
     target_class=java/lang/Object.hasNext()Z
gc::guard: …and NO live heap object holds this address in a decoded reference slot.
gc::guard: …non-moving sweep zeroed … sweep_cycle=13 free_seq=957866
     interior_off=1888 root_coverage="NEVER-LOOKED"
     xt_passes=0 xt_taken_over=0 xt_unclassified=0
gc::guard: in_published_snapshot=false published_roots=37
     last_publish_at_collection=15 collections_now=16
     holder=<not found in frames> in_blocked_region=false frames=4
     top_frame=org/h2/test/db/TestMultiThread.testConcurrentInsert
```

**`root_coverage="NEVER-LOOKED"` with `xt_passes=0`.** The sweep that freed this
block ran with no cross-thread take-over pass at all.
`stw_takeover_should_scan` gates round 0 on `any_thread_in_jit()` and runs
rounds >= 1 unconditionally, so zero passes means the barrier was satisfied at
round 0 — every thread parked cooperatively and no round ever ran. This is
exactly the reading the three-way encoding exists for: the pre-2026-08-05
counter printed only when `peers > 0` and would have rendered this as silence.

**It is a root-CAPTURE gap.** The thread published a fresh snapshot of 37 roots
for this very collection and the victim address was not among them; because it
parked cleanly nothing conservatively scanned its real stack.

> **Off-by-one warning.** `last_publish_at_collection=15` against
> `collections_now=16` looks like a one-collection-stale snapshot and is not.
> `HeapStats::minor_gc_count` is "cycles **completed**" and increments at the
> END of a cycle, so a thread publishing during collection 16's safepoint stamps
> 15. That pair is what a correct, FRESH publish looks like. This page briefly
> claimed staleness on the strength of it; do not rebuild that claim without
> re-deriving the counter's semantics.

That matches the memo defect's shape: the memo gates `scan_active_jit_frames`,
which is what the publish path enumerates from, so a suppressed detection means
that frame's oops never enter the published set.

#### A/B status — NOT a validation

Four concurrent arms, one variable, one binary. As of this writing:
`fixON = 0 guard hits / 9 iterations`, `fixOFF = 1 / 7`. Fisher's exact
p ~ 0.47. The OFF arm is simply reproducing at its historical ~1-in-8 rate and
the ON arm has not run long enough to be distinguished from it. **Nothing here
validates the fix.**

#### Throughput, checked but not controlled

The authoritative reset costs one band rescan per GC-authoritative root
collection rather than per native call. No regression is apparent: launched
together, the fix-ON arm completed **9** iterations to the fix-OFF arm's **7**
(mean minor GCs/run 25.1 vs 30.3). The arms are not length-controlled and host
load ranged 12-124, so this rules out a gross regression and nothing finer.

### A marking fail-open found while reading (2026-08-05)

`compact_oop_scan` returns `None` for an object that carries `GC_FLAG_COMPACT`
when the layout registry cannot produce its oop map. `None` is the *legacy
object* answer, so every caller — `for_each_ref_slot`, `for_each_old_gen_ref`,
`forward_ref_slots` — then reads a body of packed 8-byte fields as uniform
16-byte `Value` cells. The reference slots are never visited, so the marker
drops **every** edge out of that object and the next reclamation frees its
referents while it is live.

That is this family's exact shape, and it was indistinguishable from an ordinary
legacy object. Now counted by `COMPACT_OOP_MAP_MISSING` and printed by
`VmHeap::print_gc_summary` when non-zero. Expected zero; not yet observed
non-zero, so this is a closed hole rather than a found cause.

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

### 2026-08-05: the `NoSuchMethodError` face DID produce a reclaim verdict — in YOUNG, from a heap field

The table above records the blocked-frame face as `reclaimed_hole_at` =
**not reclaimed**, **no ring record**. That was true of every occurrence it had
then. It is not true of this one, caught on `org.h2.test.db.TestMultiThread`
with no debug flags set:

```
WARN  …vm_exec: NoSuchMethodError
      method="java/lang/Object.put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
      caller="org/h2/engine/ConnectionInfo.readProperties(Ljava/util/Properties;)V @pc=95"
ERROR …gc::guard: receiver points into RECLAIMED memory  obj="0x2005cc39d10"
      site="invoke dispatch"  location=young TO-space (the inactive semispace)
ERROR …gc::guard: …and a YOUNG non-moving sweep zeroed a span covering this
      address.  span="0x2005cc151f8+0x3ff08" interior_off=150296 sweep_cycle=5
```

Three things it settles, and one it opens.

**The region is YOUNG, not old gen.** Every verdict this page had before was
`old-gen FREE BLOCK`. This one is the inactive semispace — the from-space a
moving collection evacuated and then wiped — which is why the old-gen
reclamation ring has nothing to say about it and why every old-side instrument
in the sections above is looking in the wrong generation for it.

**The holder is a HEAP FIELD, not a frame local, a root, or a JIT register.**
`@pc=95` in `ConnectionInfo.readProperties` is:

```
84: aload_0 ; 85: getfield prop ; 88: aload 8 ; 90: aload 9
92: invokevirtual java/util/Properties.put(Object,Object)
```

so the receiver came out of `this.prop` by `getfield`, on a live
`ConnectionInfo` the frame is executing a method of. That kills the
blocked-frame framing the old page name asserted, and it also means the
reference survived *un-rewritten* rather than being *unrooted*: something wrote
a stale address into that field, or a collection moved the target and did not
rewrite the field.

**The victim's class is `java.util.Properties`.** That is the one class on this
page whose CratonVM state lives in an identity-keyed native side table
(`properties_sidetable`) — i.e. exactly the remaining candidate *Where that
leaves the residual* names, arrived at independently and from the other end.
Treat it as corroboration of that lead, not as proof: nothing yet shows the
side table held the stale address, only that the object whose field went stale
is of the type the side table backs.

What it opens: the address predates the last semispace swap, and on this
workload only **~2 moving young collections happen per run** (measured
`old_gen_scanned` 150-180 on both, i.e. during startup) with every later
collection taking the non-moving fallback — while the failing `ConnectionInfo`
is created hundreds of seconds later. Reconciling those two facts is the next
step.

`CRATONVM_DBG_STALE_OBJREF=1` is the instrument for it: it quarantines the
just-evacuated from-space instead of wiping it, so a reference that survived a
moving collection un-rewritten still carries a forwarding pointer and
`get_header` hard-panics on the first read with holder attribution, instead of
reading an all-zero header minutes later on another thread.

### A second young witness, and it narrows the question to ROOT COLLECTION

Same binary, same class, later the same day — and this one is as clean as this
family gets:

```
WARN  …gc_quiescence: [moving-young] fallback #1: reason=unregistered-jit-frame-on-stack   [18:26:21]
WARN  …vm_exec: NoSuchMethodError                                                          [18:26:28]
      method="java/lang/Object.put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
      caller="java/sql/DriverManager.getConnection(String,String,String)Connection @pc=19"
ERROR …gc::guard: receiver is inside a YOUNG span the non-moving sweep zeroed and
      returned to the free list.  obj="0x2004940a3d0" actual_class_id=0
      freed_span="0x20049406020+0x75e8" interior_off=17328 sweep_cycle=0 free_seq=2525
```

`DriverManager.getConnection(String,String,String)` is four bytecodes long
before the failure:

```
 0: new java/util/Properties ; 3: dup ; 4: invokespecial Properties.<init>()V
 7: astore_3                               <- `info`, LOCAL 3
 8: aload_1 ; 9: ifnull 20
12: aload_3 ; 13: ldc "user" ; 15: aload_1
16: invokevirtual Properties.put(Object,Object)     <- fails; @pc=19 is the `pop` after it
```

So the victim is a **brand-new `java.util.Properties`, held in LOCAL 3 of the
frame that is executing right now**, reclaimed between its constructor and its
first use. The seven seconds between the fallback line and the failure are the
pause: the thread was stopped at a safepoint across that sweep and resumed into
`put`.

**What that rules out.** `ROOT_IN_DEAD_SPANS` — unconditional, and it RETAINS
the span rather than freeing it — did **not** fire for this span. That invariant
compares the doomed spans against `roots` + `finalizer_addrs`, i.e. the exact
slice the mark phase was handed. Its silence therefore says the address was
**not in the root slice at all**. Combined with the `SWEEP_LIVENESS` heap-edge
result (`hits=0`, §above), the mark phase neither dropped a root it was given
nor missed a heap edge:

> the root slice handed to the sweep did not contain a live top-frame local of
> a thread stopped at a safepoint.

That is a **root COLLECTION** question, not a mark or sweep question, and it is
where the next instrument belongs. Note also that all three frame-root paths
already screen operand-stack roots with `is_heap_addr` rather than the strict
`is_object_address` header probe (see `scan_frame_roots`' doc comment), so the
mid-init-object hole that shape would otherwise have is already closed — this
witness is on the fixed code.

`sweep_cycle=0` is worth keeping too: this is the FIRST non-moving young sweep
of the process. Whatever the gap is, it does not need a long-running heap or an
accumulated free list to appear.

### The `hasNext()` witness, reproduced WITH the provenance answer (2026-08-05)

The face the old page was named for — and this time it says where the address
stood in the owning thread's own bookkeeping:

```
WARN  …vm_exec: NoSuchMethodError method="java/lang/Object.hasNext()Z"
      caller="org/h2/test/db/TestMultiThread.testConcurrentUpdate()V @pc=252"
ERROR …gc::guard: receiver points into RECLAIMED memory  obj="0x2004621fa78"
      location=young from-space FREE BLOCK (reclaimed)  span="0x2004621c7a0+0x3dd0"
ERROR …gc::guard: …and this is where that address stood in the OWNING thread's own
      GC bookkeeping.  in_published_snapshot=false  published_roots=38
      in_blocked_region=false  frames=4
      top_frame=org/h2/test/db/TestMultiThread.testConcurrentUpdate pc=252
```

Read the three of them together:

* `location=young from-space FREE BLOCK (reclaimed)` — not "past the frontier",
  not the inactive semispace. The address is inside a free block of the
  CURRENT from-space right now, which is the one answer with no false
  positives: a live object is never there;
* **`in_published_snapshot=false`** — the snapshot the collector marks this
  thread from does not contain the address, while the thread's own top frame
  holds it. `published_roots=38`, so the snapshot exists and is populated; the
  slot is simply not in it;
* `in_blocked_region=false`, `frames=4`, and the failing frame is the TOP
  frame — so this is not the parked-thread deposit path at all. It is a
  counted, running mutator whose top frame is the one holding the dangling
  reference.

Together with `ROOT_IN_DEAD_SPANS` and `SWEEP_LIVENESS` both silent (§above),
that is three independent instruments agreeing: the mark did not drop a root it
was handed, no heap edge pointed into the doomed span, and the root slice never
had the address. **The gap is in publishing, not in marking, sweeping, or
delivery.**

The next number to get is how OLD the snapshot the collector used was:
`report_root_slice_provenance` now also prints `last_publish_at_collection`
against `collections_now`, stamped by `note_root_publish` at both publish sites
(`update_root_snapshot` and `deposit_root_snapshot_inner`). A non-zero
difference means at least one collection completed after this thread last
published — i.e. it was marked from a snapshot that could not contain anything
allocated since. That is the shape both young witnesses have, and it has a
plausible mechanism: the publish hook fires on object-RETURNING native calls,
so a stretch of bytecode that allocates and then calls only void natives (or no
native at all) never republishes.

### The `Iterator` was never the victim — the reported method name is an ARTIFACT

Resolve the pc first, because it is what this whole page was named after.

**The caller pc a dispatch failure reports is the POST-invoke pc**, i.e. the
return address, not the call site. That is not an inference from the numbers —
the interpreter advances the frame's pc BEFORE it dispatches, and the terminal
report reads `thread.frames.last().pc`:

```rust
// invokeinterface — stackless dispatch with monomorphic inline cache
0xb9 => {
    // invokeinterface is 5 bytes: opcode(1) + index(2) + count(1) + 0(1)
    thread.frames[frame_idx].pc = saved_pc + 5;
    let cached_result = execute_invokevirtual_cached(…, saved_pc, …);
```

Three witnesses, three builds, all consistent with it:

| witness | call site | reported pc | site + length |
| --- | --- | --- | --- |
| `TestMultiThread.testConcurrentUpdate` | `invokeinterface ExecutorService.shutdown()V` @247 | **252** | 247 + 5 |
| `TestMultiThread.testConcurrentInsert` | `invokeinterface ExecutorService.shutdown()V` @192 | **197** | 192 + 5 |
| `DriverManager.getConnection` | `invokevirtual Properties.put(…)` @16 | **19** | 16 + 3 |

So the two H2 witnesses this family is named for did **not** fail in the
`for (Future<Void> job : jobs)` loop. `@pc=252` is `aload 4` immediately after
`executor.shutdown()`; `@pc=197` is `aload_3` immediately after the identical
call in the sibling method. Both failing call sites are
`invokeinterface java/util/concurrent/ExecutorService.shutdown()V`, and the
victim is the **`executor` local** — slot 4 in `testConcurrentUpdate`, slot 3 in
`testConcurrentInsert` — which is live from its assignment to the end of the
method, not a loop temp.

**Then the reported method name is wrong.** The call site names
`shutdown()V`; the report says `java/lang/Object.hasNext()Z` (and
`java/lang/Object.next()Ljava/lang/Object;` for the sibling). A `ClassId(0)`
receiver explains the CLASS — `java.lang.Object` is class id 0 — but nothing
about a zeroed receiver renames `shutdown` to `hasNext`. All three of
`shutdown()V`, `hasNext()Z` and `next()Ljava/lang/Object;` are
`invokeinterface` with `count=1`, and `hasNext`/`next` are what the loop
earlier in the same method dispatches, so an interface call-site cache that
mixes entries when the receiver class id is 0 produces exactly this. That is a
second, separate defect and it is not established here — but it is why the
`java/lang/Object.put(...)` witness in `DriverManager.getConnection` reported
the RIGHT name: that one is an `invokevirtual`.

Two consequences worth stating plainly:

* **the old page name — and three sessions of reasoning about a synthetic
  iterator local held only by a parked frame — rests on that artifact.** The
  actual victim is an ordinary, long-lived local of the running method;
* the `blocked=false` in the original 2026-08-02 receiver dump was right and
  should have been believed: the thread is not parked when it trips, and
  `in_blocked_region=false` in the 2026-08-05 provenance line says so again.

### Two more `TestMultiThread` faces worth counting, and one arm that cannot be soaked

Faces seen on the 2026-08-05 campaigns beyond the four this page lists. Neither
is established as this family; both are recorded so a future campaign counts
them instead of dismissing them as application flakiness:

* `General error: "java.lang.NullPointerException: Cannot invoke
  ""org.h2.result.ResultInterface.isLazy()"" because ""result"" is null"` — a
  reference field reading NULL. Note that a field read off a ZEROED object
  returns 0, which decodes as `null`, so this is a plausible face of the same
  defect one step downstream of `ClassId(0)`;
* `The database has been closed` mid-run with 26 live connections. H2 closes a
  database when its last session unregisters, so this is what losing an entry
  from `Database.userSessions` looks like from the outside.

**`CRATONVM_DBG_NO_NONMOVING_RECLAIM=1` cannot settle them.** The intent was a
clean discriminator — with the non-moving sweep's dead spans neither zeroed nor
published, a reference the root scan missed keeps its original header, so the
failure should vanish if it really is reclamation. Two runs at `--Xmx 3g`: the
first still failed (the `result is null` NPE, 40 s in), the second died of
`OutOfMemoryError`. The flag defers only the NON-MOVING sweep, so selective
promotion still evacuates and the moving path still resets from-space; and the
unbounded leak ends the run before much sweeping happens. So it is neither a
clean negative nor soakable — do not read the first run as exoneration.

### ROOT CAUSE (2026-08-06): a SUB_OBJECT slot the encoder minted and the decoder refused

`CompactValue` NaN-boxes a reference as a `SUB_OBJECT` slot. Because a 64-bit
`long` can carry the identical bit pattern, the *context-free* decoders
(`CompactValue::to_value`, `crate::value::decode_value`) do not trust the tag
alone: they ask `crate::value::object_ref_payload_is_known(payload)` -- a
process-wide bitmap of every payload that has crossed a reference-construction
boundary. An unknown payload is **degraded to `Value::Long`** and counted by
`object_degradation_count()`.

Only `ObjectRef::from_raw` / `from_raw_nonnull` recorded into that bitmap. The
four *encoders* of a `SUB_OBJECT` payload did not:

| encoder | who calls it |
| --- | --- |
| `CompactValue::object(raw)` | `local_slot_to_compact`, `RawSlot` bridge, SoA thaw |
| `CompactValue::try_from_pointer(raw)` | `Frame::update_object_refs` (GC relocation) |
| `CompactValue::update_object_ptr` | GC compaction scanner |
| `CompactValue::update_object_ptr_unchecked` | GC compaction scanner (hot path) |

So the encoder could mint a slot its own decoder refused. What follows is the
whole family:

1. a live reference lands in a frame slot through one of those encoders with a
   payload the bitmap does not know;
2. the next context-free decode returns `Value::Long`, and any `Value`-typed
   store of that result marks `Frame::local_kinds[i] = LKIND_LONG`
   (`ValueStack::kinds` for an operand-stack slot);
3. `Frame::scan_local_objects` **skips LONG slots by design** -- the root is
   never published. That is the measured `in_published_snapshot=false` on a
   RUNNING mutator whose top frame holds the address, with
   `ROOT_IN_DEAD_SPANS=0` and `SWEEP_LIVENESS hits=0`: nothing dropped the root,
   the root was never handed over;
4. the sweep reclaims the object. What the mutator reads back next depends on
   what overwrote the span -- a zeroed span reads as `ClassId(0)` (class id 0 is
   `java.lang.Object`), a span carrying free-list metadata fails
   `is_object_address` and `coerce_value_for_return_validated` turns the
   reference into **`null`**.

`bytecode` never consults `local_kinds`, so step 2 is invisible from Java: the
slot keeps loading and storing correctly right up to the moment the GC runs.

**The evidence that named it.** `TestMultiThread` x32 on
`cratonvm-h2cid0-slot-20260805`: the one-shot
`CompactValue: first long<->object NaN-box collision degraded to Value::Long`
line and the `"result" is null` failure face co-occur **perfectly** -- 6 runs
with both, 26 runs with neither, and every remaining failure was a
`TimeoutException` / `Timeout trying to lock table` on a host at load 90 (i.e.
harness noise, and degradation-free). Two competing explanations were killed in
the same campaign: `CRATONVM_DBG_ROOTSNAP_VERIFY` reported
`miss_snapshots=0 missed_roots=0` over every snapshot of several runs, and the
face still reproduced under `CRATONVM_ROOTSNAP_CACHE=0`, so the frozen-frame
root-snapshot cache is exonerated.

**Why it looked like three different bugs.** The `"result" is null` face is
`Command.executeQuery`'s `ResultInterface result = query(maxrows);` -- a local
assigned straight from a call return and used one bytecode later, so nothing
about it is a GC *timing* window. It is the same slot corruption arriving
through the return-value path.

**The fix** records the payload inside all four encoders, so the invariant
"anything the VM encoded as a reference decodes as a reference" holds by
construction instead of by convention. Three sites
(`Frame::update_object_refs` x2, `ValueStack::update_object_refs`) had already
been hand-patched with a `ObjectRef::from_raw` round-trip whose comments state
this exact mechanism -- three hand-patches of one invariant is the signal that
the invariant belongs one level down. Those round-trips are now redundant and
harmless. Regression test:
`every_sub_object_encoder_records_decodable_provenance_cv`.

### Young-side hypotheses closed with measurements (2026-08-02 → 08-05)

Recorded so they are not re-derived; each cost a build-and-soak cycle. These
are the young-generation counterparts of the old-gen negatives above.

1. **The young non-moving sweep does not drop a root it was handed.** The
   root-in-dead-span invariant (`ROOT_IN_DEAD_SPANS`, unconditional, and it
   RETAINS the span rather than freeing it) reported **zero** violations across
   thousands of sweeps under `CRATONVM_NO_MOVING_YOUNG=1
   CRATONVM_DBG_GC_STRESS=2097152`, once roots pointing at an EVACUATED source
   were excluded. That exclusion is the measurement: before it the check fired
   constantly and every hit was a promoted object whose young source shares a
   dead run with an unpromoted neighbour. A dead span is a RUN of consecutive
   dead objects carrying the first one's header — ask `is_forwarded()` of the
   ROOT's object, not the run's head, or the check is pure noise.
2. **No heap edge points into a doomed young span either.**
   `CRATONVM_DBG_SWEEP_LIVENESS=1` on the real class: 14+ non-moving sweeps,
   `hits=0 root_hits=0` on every one, ~611 000 old-gen objects scanned per cycle
   against 32 000-60 000 doomed spans. Its young→young half stays vacuous
   (`young_survivors_scanned=0`) because selective promotion moves every
   survivor out — a real limit of the assertion, not a clean result.
3. **`Object.clone()` is not a use-after-move.** `native_object_clone` reads
   fields off `this` AFTER `ctx.alloc_object`, which is the exact shape of the
   native stale-local bug class — but `NativeContextImpl::alloc_object` never
   collects ("a native callback never initiates collection on this path", its
   own comment), and a counter that fired whenever the receiver moved across
   that allocation read **0**, including under `CRATONVM_DBG_GC_STRESS`. The
   pin/read/unpin hardening was written, measured, and reverted rather than
   left as unjustified cost on `MVStore.Page.copy()`, which clones a page per
   structural modification.
4. **Per-bci local liveness is correct for the enhanced-for shape** the
   `hasNext()` witness came from. The synthetic `Iterator` local is read only
   across the loop's BACK EDGE, so an analysis that did not reach a fixpoint
   over it would report the slot dead exactly where the thread parks — and that
   mask filters a blocked thread's root snapshot. Regression test:
   `local_liveness::tests::enhanced_for_iterator_is_live_at_the_blocking_call`,
   at the real byte offsets of `TestMultiThread.testConcurrentUpdate`.

Two instrument lessons from the same window, both of which cost a build:

* **A primitive array reads back class id 0.** Array headers carry the
  COMPONENT class id (JVMS §4.4.1) and `long[]`/`int[]` have none, so a
  `ClassId(0)` gate flags every `long[] toc` local in
  `FileStore.dropUnusedChunks` on every wake. Gate on `kind == Object` too.
* **Filter frame locals by the frame's own live mask.** The frame audit's first
  true hit was `H2ConcurrentUpdateLoop.main` local 8 pointing into a young free
  block — the seed loop's `PreparedStatement`, semantically dead for the rest of
  the method, i.e. the liveness filter working exactly as designed.

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

* `../jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`
  — the JIT miscompile that made this family's reproduction impossible, and the
  reason a wrong-object return has to be excluded before a stale reference is
  assumed. **Read this before attributing anything here to GC.**
* `../../../gc/old-sweep-liveness.md` §7 — the interior-conservative-root fix,
  its counters and its negative control.
* `../h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`
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
`../hibernate/smoketests-stale-pointer-nosuchmethod-crash-20260804-RETIRED.md`.
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

---

# REOPENED 2026-08-07: the barrier rewrites a live reference

Found while retiring
[`../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md).
That page had been carrying an H2 failure it attributed to a dispatch bug; the
attribution was wrong and the failure is this family's, in a face that page's
instruments could see and this page's could not.

## The reproducer

`org.h2.test.db.TestTempTables`, `--java-home <jdk25> --Xmx 1g --nojit`, one
class per process, 6 concurrent workers. **4 events in 46 runs (~9 %)**, against
0 in 30 JIT-on runs of the same class. A passing run is 500-950 s on a loaded
host; a failing one aborts at 90-850 s. No debug flag needed — the verdict is
unconditional.

This is a materially better vehicle than `TestMultiThread` (~1 face in 8 iters)
or `TestMVStoreCacheLoop` (~1 event per 3 worker-hours): single-threaded test
logic, no randomisation, and the failure has a fixed call site.

## What it looks like

```text
java/lang/ClassCastException: jdk.internal.misc.InnocuousThread cannot be cast to [J
	at org/h2/mvstore/tx/TransactionStore.registerTransaction(TransactionStore.java:499)
	at org/h2/mvstore/tx/VersionedBitSet.<init>(VersionedBitSet.java:25)
	at org/h2/mvstore/tx/BitSetHelper.flip(BitSetHelper.java:34)
	at java/util/Arrays.copyOf(Arrays.java:3617)          <- innermost
```

`Arrays.copyOf(long[], int)` at pc 7 is `invokevirtual "[J".clone:()Ljava/lang/Object;`.
Before 2026-08-07 the same event surfaced as `CloneNotSupportedException` from a
`java/lang/Thread.clone` frame, because the VM dispatched on the receiver's
header and ran `Thread.clone`'s body; array-typed call sites now resolve
statically per JVMS §4.4.1, so the `checkcast [J` reports the object instead.

Four occurrences, all with `kind=Object`, `gc_age=2`, `gc_flags=0x01`
(old generation), all **live, valid** objects:

| receiver address | `receiver_class` |
|---|---|
| `0x20010039668` | `jdk/internal/misc/InnocuousThread` |
| `0x200100e37f8` | `org/h2/mvstore/FileStore$BackgroundWriterThread` |
| `0x200100e1dd0` | `java/lang/Thread` |
| `0x2001003bb50` | `java/lang/Thread` |

All four are `java.lang.Thread` or a subclass. Nothing about the H2 call site
selects for that, so it is a property of *which memory the bad reference lands
in*, and it is a lead: these are early-promoted, long-lived old-gen objects.

## The measurement this page never had

`vm/src/memory/reclaim_guard.rs`'s new `report_impossible_dispatch_terminal`
records the receiver **as the operand stack handed it over**, before
`execute_invoke_kind`'s forwarding read barrier runs:

```text
pre_refresh_obj="0x20042853308"   barrier_rewrote=true
pre_refresh_kind=Array            pre_refresh_class=java/lang/Object
pre_refresh_header="[00,00,00,00, 01, 0b, 00, 00, 06,17,ce,01, 01,00,00,00, 00×8]"
holder=frame#14 org/h2/mvstore/tx/BitSetHelper.flip pc=38 local[0] kind=0 live=true
object_degradations=0
```

Decoded against `ObjectHeader`: `class_id=0`, `kind=Array`,
`element_type=Long`, `gc_age=0`, `gc_flags=0`, `shape=1`. **A live, intact
`long[1]`** — the `VersionedBitSet.bits` the call site is holding — and
`BitSetHelper.flip`'s `local[0]` still points at it, live, at the moment of the
report.

So this occurrence is **not** a lost root, **not** a missed remap, and **not** a
`CompactValue` degradation (`object_degradations=0`). The frames are correct.
The one statement between the correct value and the wrong one is

```rust
// vm/src/runtime/interpreter/invoke.rs, execute_invoke_kind
*obj = shared.mem.heap.load_and_forward(*obj);
```

which read `0x20042853308` and returned `0x2001003bb50`.

`VmHeap::load_and_forward` (`gc/src/vm_heap.rs`) does exactly one thing: load
the source's `mark_word` (relaxed), and if `mark & 0b11 == MARK_FORWARDED`
follow the upper 62 bits as a relocation target, accepting any destination
`is_object_address` likes — which a re-served old-gen `Thread` satisfies.

## The open question, stated precisely

By the time the report ran, that same source header read
`mark_word = 0` (`MARK_NEUTRAL`). So either

* **the barrier read a genuine forwarding word that was afterwards cleared** —
  in which case the target was *wrong* (a `long[1]`'s relocation target is not a
  `Thread`), and the suspect is whoever installs forwarding on a young source:
  `gen_heap.rs`'s selective-promotion `fwd_installs` loop (which installs
  deferred `(src_addr, dst)` pairs collected on "anchor-verified stretches", and
  unwinds suspect stretches — an install/unwind window is exactly the shape of a
  marker that is present at one instant and gone at the next), or the moving
  collector's `set_forwarding_address` at `gen_heap.rs:13055`;
* **or the barrier's own relaxed load disagreed with a later read of the same
  word**, which would make this a visibility/tearing problem rather than a
  bookkeeping one.

Nothing currently in the tree distinguishes them, because the barrier's decision
is not recorded anywhere and the word is mutable behind it.

## Next step — already built, needs a run

`reclaim_guard::note_barrier_rewrite` / `last_barrier_rewrite` capture
`(source, mark word AS THE BARRIER READ IT, destination)` in a thread-local at
the moment the barrier rewrites, and the verdict prints them as
`barrier_src` / `barrier_mark_at_read` / `barrier_mark_state` / `barrier_dst`
alongside `pre_mark_now`. That is a one-line discriminator:

* `barrier_mark_state == 3` → a real forwarding marker with a wrong target.
  Go to the install sites above.
* `barrier_mark_state == 0` → the barrier acted on a word that was never a
  forwarding marker. Go to the load, not the installer.

It is in the tree and compiles; it had not fired at the time of writing (0
events in the 18 runs after it landed — the event rate is load-sensitive and the
host had quietened). Re-run the reproducer above and read that line first.

## Related

* `docs/internal/repros/h2-clone-spin-20260807/CloneSpinProbe.java` — the
  negative control. It drives the identical `BitSetHelper.flip` →
  `Arrays.copyOf` → `original.clone()` shape with the database removed: **28.4 M
  executions of the failing bytecode in 90 s, zero failures.** The call shape is
  not the variable; H2's heap is.
* `docs/internal/repros/h2-insert-scale-20260731/H2InsertScaleProbe.java` at
  `25 1000` — the retired page's 2026-07-31 residual, recorded there as failing
  all 25 threads. 16 runs on current `dev`, `failed=0` every time. Withdrawn as
  an independent data point.
