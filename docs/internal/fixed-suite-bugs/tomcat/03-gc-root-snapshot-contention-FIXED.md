# Group 03 — GC root-snapshot lock contention / per-call cost  (FIXED, partial)

**Status:** FIXED (the contention explosion), merged to `dev`
(`90ce03eb` commit `2034b4a9`; `44db1836` commit `92efb6ac`).
Residual call-frequency cost remains — see [04](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md).

## Symptom

Embedded-server webapp deployment ran ~150s+ (HotSpot <1s) and appeared to
"hang." Quantified with a gated counter (`CRATONVM_DBG_ROOTSNAP`, committed) on
`update_root_snapshot`:

```
calls=1.8M  total_ms=6,711    avg_us=3.7    avg_frames=56   (testSimpleSsl)
calls=2.0M  total_ms=145,935  avg_us=73     avg_frames=58
calls=2.2M  total_ms=790,850  avg_us=359    avg_frames=56
```

i.e. `update_root_snapshot` is called on EVERY object-returning native call
(millions of times), scanning a ~57-deep stack each time, and the per-call cost
**exploded ~1000×** (3.7µs → 3,225µs and climbing) at CONSTANT depth.

## Root cause

`update_root_snapshot` validates every operand-stack object via
`GenerationalHeap::is_object_address`, whose region-containment check took
**three arena mutexes** (`young_from.lock() || young_to.lock() ||
old_gen.lock()`). Per operand object × per frame × per native call, this
contended catastrophically with the concurrent GC/allocator as the heap filled.
(`OldGen::contains` itself is O(1), so the blowup is lock contention, not
algorithmic.)

## Fix (`gc/src/gen_heap.rs`)

Cache each region's `[base,end)` in `region_bounds: [(AtomicUsize,AtomicUsize);
3]`; the containment check now does lock-free Acquire loads. Bounds republished
(Release, under held guards) at construction + GC start/end — the only points
the young arenas swap/grow (`young_to.grow` is on the EMPTY to-space near GC end;
the swap exchanges two already-bounded ranges; old gen never reallocates). GC is
STW, so mutators read only between cycles → no false negatives (which would drop
a root). A second commit made the operand-stack validation allocation-free
(in-place compaction vs per-frame `split_off` Vec alloc).

## Result

Contention explosion **gone** — per-call cost flat at ~45µs (declining) vs
exploding past 3,225µs; ~10× more native calls processed per wall-second. bt
checksums unchanged (bt18=68332206); no stale-ref SEGV over 22M calls.

## Residual (NOT fixed here)

`update_root_snapshot` is still called per object-returning native call (tens of
millions during a deploy) × O(stack-depth) scan. The raw call VOLUME is now the
bottleneck (~45µs × millions), addressed only by caching the caller-frame scan
(O(depth)→O(1)) or reducing publish frequency — both GC-correctness-critical and
deferred. See [04](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md).
