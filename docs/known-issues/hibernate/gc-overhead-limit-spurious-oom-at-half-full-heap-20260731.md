# Spurious `OutOfMemoryError` with 570 MB free — the GC-overhead limit latches on a 49%-full heap that never promotes

| | |
|---|---|
| **Status** | 🔴 OPEN — real VM defect. Proximate cause identified and confirmed by differential; the underlying cause is the already-OPEN moving-young fallback. |
| **ID** | `HIB-GCOVERHEAD-HALFFULL.1` |
| **Found** | 2026-07-31, validating the `DefaultCatalogAndSchemaTest` runner accommodation. |
| **Repro** | `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`, JIT **on**, `--Xmx 1500m`, real JDK. OOM at ~41 min, 3 for 3. |

## Symptom

```
Exception in thread "main" java/lang/OutOfMemoryError: Java heap space
    (anewarray component 6 length 644)
```

A 644-element reference array — roughly 5 KB — fails to allocate. HotSpot runs
the identical class, classpath and heap in **119.7 s**, `found=132 ok=132
failed=0`, clean.

## The heap is half empty when it fires

`CRATONVM_DBG_GC_OVERHEAD=1`, last eight of thirty forced GCs before the OOM:

```
before=570348120 after=553940032 promoted=0 freed=16408088 cap=1179648000 unproductive=true streak=2
before=556936120 after=554082928 promoted=0 freed=2853192  cap=1179648000 unproductive=true streak=3
before=557079344 after=554224496 promoted=0 freed=2854848  cap=1179648000 unproductive=true streak=4
before=557345504 after=554367544 promoted=0 freed=2977960  cap=1179648000 unproductive=true streak=5
before=557484232 after=554509568 promoted=0 freed=2974664  cap=1179648000 unproductive=true streak=6
before=557515960 after=554651760 promoted=0 freed=2864200  cap=1179648000 unproductive=true streak=7
before=557649104 after=554788704 promoted=0 freed=2860400  cap=1179648000 unproductive=true streak=8
```

- Live set **554 MB** of a **1125 MB** cap — the heap is **49 % full**, with
  ~570 MB free, when the allocator gives up on 5 KB.
- `promoted=0` on **all thirty** forced GCs. Not one byte was promoted.
- Each cycle frees ~2.8 MB = 0.24 % of capacity, so `unproductive=true`
  latches eight times and `gc_overhead_limit_exceeded` turns the next
  allocation failure into a pre-allocated `OutOfMemoryError`.

The productivity accounting itself is **correct** — this is *not* a recurrence
of the `allocated_bytes`-vs-`live_bytes_estimate` bug fixed 2026-07-27
(`live_bytes_estimate` is wired at `vm/src/runtime/interpreter.rs:1615`/`:1767`,
and `before`/`after` visibly move here). The metric is honest; the *threshold*
is being applied in a situation its author explicitly did not anticipate.

## Differential: the limit is what kills it

One variable changed, same binary, same everything else:

| arm | outcome |
|---|---|
| default | **OOM at ~41 min**, 104 of 132 tests reached |
| `CRATONVM_GC_OVERHEAD_LIMIT=0` | **no OOM at 85 min**, 111 of 132 tests reached, still running |

(progress measured as `HHH000490: Using JTA platform`, one per test's
`SessionFactory`; HotSpot's control run logs all 132.)

So the heap is genuinely never exhausted — disabling the limit lets the run
continue past the point where it otherwise dies.

## Why `note_gc_productivity`'s reasoning does not hold here

`vm/src/runtime/interpreter.rs::note_gc_productivity` deliberately scores
*freed bytes* rather than post-GC fullness, and says why:

> *"in a retained-allocation death-spiral the young semi-space is emptied every
> cycle (so total fullness sits near young/total ≈ 50 % and never looks
> exhausted), yet the GC frees ~nothing net because every survivor is promoted
> into an already-full old generation … a wedged, ~full old generation cannot
> absorb 2 % of total heap capacity per cycle"*

That reasoning is sound for the spiral it describes, and it is exactly why a
fullness gate was rejected. But its distinguishing feature is *promotion
pressure into a full old gen* — and here **`promoted=0` on every cycle and the
old generation is not full**. The heuristic cannot tell the two apart, so it
reads "small amount freed" as "wedged" when the truth is "nothing needed
promoting".

The companion comment — *"Forced GCs only happen on genuine allocation failure
(young full **and** promotion blocked), so this never fires during ordinary
young-GC churn"* — is also not holding: thirty forced GCs fired while the heap
sat at 49 %.

## Underlying cause: the non-moving sweep never compacts

Every young collection in these runs is diverted away from the copying
collector:

```
[moving-young] fallback #N: reason=compiled-frame-oop-not-published — a live JIT
frame could not prove a complete rewritable root map, so this young collection
runs the NON-MOVING sweep (no compaction, free-list allocation).
```

Reason histogram over the JIT-on runs: `compiled-frame-oop-not-published`,
`unregistered-jit-frame-on-stack` (6), `missing-exact-rbp` (3),
`active-safepoint-map-incomplete` (1).

A sweep that reclaims into a free list without compacting fragments the young
generation. That is the only self-consistent explanation for the whole picture
at once: ~570 MB free, no contiguous 5 KB span, `promoted=0` (nothing drains),
and ~2.8 MB reclaimed per cycle. It is also why the run *slows down* as it goes
— 104 tests in the first 41 min, then only 7 more in the next 44 min.

This is the same mechanism already tracked in
[`moving-young-inert-under-jit-throughput-tax-20260730.md`](moving-young-inert-under-jit-throughput-tax-20260730.md)
and its authority
[`../../internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`](../../internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md).
Those document it as a **throughput** tax; this doc records that on a
long-running allocation-heavy class it is also a **correctness** failure — the
process dies with an `OutOfMemoryError` on a half-empty heap.

## Suggested direction

Two independent layers, fixable independently:

1. **The real fix** is the moving-young coverage gap: four separate obligations
   (`compiled-frame-oop-not-published`, `unregistered-jit-frame-on-stack`,
   `missing-exact-rbp`, `active-safepoint-map-incomplete`) each divert the
   collection, so the copying collector effectively never runs under the JIT.
   Owned by the docs linked above.
2. **A cheap mitigation** for the spurious OOM: `note_gc_productivity` should
   not latch a cycle as unproductive when the heap has ample headroom *and*
   nothing was promoted — the combination that distinguishes "fragmented but
   roomy" from the "wedged full old gen" spiral the threshold was written for.
   HotSpot's own `UseGCOverheadLimit` requires both a GC-time fraction **and** a
   free-space condition; CratonVM currently checks only the freed-bytes half.
   This would convert the OOM back into (severe) slowness rather than a crash,
   which is the honest failure mode while layer 1 is open.

Do not "fix" this by raising `--Xmx`: the limit is keyed to a *fraction* of
capacity, so a bigger heap raises the 2 % bar proportionally and the streak
still latches.
