# `MVStore` cache read returns `java.lang.Object` — an OLD-GEN block reclaimed while still referenced

> The filename still says `nonmoving-sweep` because code comments and a sibling
> report point at it. The name is wrong; see *Status*.

## Status
**OPEN — one named mechanism closed, the family is not.** Updated 2026-08-03
on `fix/h2-classid0-close-20260803`.

* **A root cause was found, fixed, and differentially tested.** `old_gen_gc`'s
  root seed asked only ever "is this address an object BASE?", of every root,
  twice — and a conservative root is frequently an *interior* word. Old gen had
  no resolution for that at all, so an old-gen object whose only surviving
  reference was an interior word got **no mark bit**, and the in-place sweep
  frees purely on `GC_FLAG_MARKED`. The compacting arm was additionally
  *assumed* unreachable with conservative roots and measured not to be. Full
  argument, counters and the negative control: `docs/gc/old-sweep-liveness.md`
  §7. Two regression tests, one per reclamation arm, each verified to FAIL
  under `CRATONVM_GC_NO_OLD_INTERIOR_PINS=1` — the second with this family's
  own face, `address 0x… now reads class_id=0`.
* **It is not the whole defect.** On a 2026-08-03 A/B soak the FIX arm still
  produced a live occurrence, and the verdict says it was **not** interior-rooted:

  ```
  receiver is an OLD-GEN block this process RECLAIMED while it was still referenced.
    obj=0x20028f6a4e8  site="JIT checkcast"  target_class=java/lang/String
    original_class=java/lang/Object  original_kind=1        <- an ARRAY
    freed_block="0x20028f691c8+0x2020"  interior_off=4896
    interior_root_pointed_in=false
    freed_by="in-place old-gen sweep"   free_seq=1888514
  ```

  An `Object[]` of ~8 KB, freed by the in-place old sweep under a live
  reference, then re-served — surfacing to Java as
  `java.lang.Integer cannot be cast to java.lang.String`. So at least one more
  mark-phase gap remains, and it is on the in-place arm.
* **Every earlier measurement in this family needs re-taking.** A JIT
  miscompile that hands the WRONG OBJECT back from a virtual call was live on
  `origin/dev` for the whole history of this investigation and is fixed on this
  branch (`12769bb23c`, see
  `../../internal/fixed-suite-bugs/jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`).
  At the reader end a wrong-object return is **indistinguishable** from a stale
  reference. It is not the explanation for the verdict quoted above — that one
  is the heap's own free-list answer, not an inference off a cast — but it is a
  live alternative explanation for any occurrence recorded without one.

The root cause of the residual is not known. What this session leaves behind is
one mechanism closed with tests, a reproduction that still works, verdicts that
need no prior configuration on three faces instead of one, and the removal of a
confound that was corrupting the evidence.

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

### What that leaves

The blocks this sweep frees are unreachable from **everything the collector can
see**: the whole old generation, the whole young space, and the entire root
slice. Yet two independent fix-arm witnesses show such a block being read back
later through a surviving reference.

So the surviving reference is somewhere the collector never looks. In rough
order of suspicion:

1. **A peer thread's JIT spill slots.** These runs log
   `[moving-young] fallback: reason=compiled-frame-oop-not-published` and
   `reason=innermost-rbp-belongs-to-unguarded-callee` — the collector knows some
   live JIT frames cannot publish a precise map. The non-moving fallback means
   it does not have to *relocate* them; it still has to **mark** through them.
   The next measurement is root COVERAGE, not mark completeness: per old-gen
   sweep, how many threads were in JIT and how many of their frames contributed
   roots.
2. **A native/Rust side table.** `native-collections` keeps state in identity-keyed
   overlays (`clone_lhm_overlay`, `properties_sidetable`) and `external_roots`
   registers owners. `CRATONVM_DBG_OLDSWEEP_OWNERS=1` already reports when a
   freed block **is** an overlay owner; nothing reports when a freed block is
   **referenced by** one.
3. **A thread whose snapshot was not folded into this cycle.**

This is a genuine reframing: for three sessions the working hypothesis has been
"a gap in the old-gen mark phase". The mark phase is now measured complete over
every edge that exists in the heap. The defect is in **what the collector is
told about**, not in what it does with it.

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

## Symptom

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
- the retired `bug-h2-testdiskfull-classid0-corruption-segv-cce` write-up —
  same signature; see *Handed over from `TestDiskFull`* above.
- `c0d09e2451`, `b86945eafe` — post-GC reference processing writing through
  stale OLD-GEN addresses, and the same blind spot in its staleness guard.
  Already on `dev` before the reproduction here, so they do not close this, but
  they are the same generation and the same shape.
- `5750caf5f` — *close the live set before the in-place old sweep decides what
  is dead*: the in-place sweep now runs the compactor's live-set fixpoint, and
  the seven PRECISE mark push sites stop being screened by a plausibility test
  written for conservative guesses. Its counters
  (`OLDMARK_RESCUED_BY_WALK`, `OLD_SWEEP_CLOSURE_RESCUES`,
  `OLD_SWEEP_ESCAPE_HITS`) are the ones to watch for a recurrence.
- `7303483521` fixes the old-gen **fragmentation** consequence of the same
  regime. Unrelated cause, unrelated fix — that part of the original doc was
  right.

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
