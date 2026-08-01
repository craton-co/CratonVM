# `MVStore` cache read returns `java.lang.Object` — an OLD-GEN block reclaimed while still referenced

> The filename still says `nonmoving-sweep` because code comments and a sibling
> report point at it. The name is wrong; see *Status*.

## Status
**RE-DIAGNOSED 2026-08-01 — the title of this doc is wrong.** Reproduced,
and the reclaimed object is in the **old generation**, not a young-sweep span.
The fix for that gap (`5750caf5f`, *close the live set before the in-place old
sweep decides what is dead*) landed on `dev` the same day from a concurrent
line of work and is merged here; a contemporaneous A/B soak against a pre-fix
binary is what remains before this can be called closed. Everything below is
measurement, not inference — including the parts that say a previous
measurement was misread.

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

| driver / binary | worker-hours | events |
| --- | --- | --- |
| `TestMVStoreCacheLoop`, `origin/dev` @ `c8a3ba181d` | ~6.3 | 2 (1 CCE + 1 SIGSEGV) |
| `TestMVStoreCachePerformance` (stock), same binary | ~5.5 | 0 |

`org.h2.test.synth.TestDiskFull` — offered by the now-retired
`bug-h2-testdiskfull-classid0-corruption-segv-cce` write-up as a 2-second
reproducer for this family — produced **0 SIGSEGV and 0 CCE in ~110 runs**
across three binaries on the same day, so it is not a usable handle right now.
See *Handed over from `TestDiskFull`* below.

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
