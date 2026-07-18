# TLAB-trigger GC exposes young-walk/free-list corruption (bt18 under-count)

Status: OPEN. Found 2026-07-18 during the perf/halfgap-20260717 round.
The exposure vector is quarantined (`CRATONVM_TLAB_GC_TRIGGER` default OFF);
the underlying defect is latent on dev and pre-dates this round.

## Repro (deterministic-ish, ~4/5 runs)

```
CRATONVM_TLAB_GC_TRIGGER=1 cratonvm --java-home <jdk25> -cp bench BinTreesClassic 18
# correct checksum: 68332206 — corrupt runs print 67644084 / 68168370 / 68201136 /
# 67676850 (UNDER-counts: live nodes reclaimed), plus WARNs:
#   "young walk: cursor <x> overshot into free block [a, b) — walk grid / free list disagree"
#   "non-moving sweep: stopping walk at offset <o> — implausible object size ... class_id=0"
#   "[quiesce] FIRST corruption: quiescence depth=15 enter_count=..."
```

With the flag off (default), the same binary produces 68332206 every run
(the collections that would expose the defect simply don't happen: the only
young GC fires at first bump-exhaustion, before the free list has split
remnants).

## What the flag does

`CRATONVM_TLAB_GC_TRIGGER=1` enables two triggers in
`tlab_alloc_object_inner` (vm/src/runtime/interpreter.rs) added to break the
"crumb treadmill": (a) a wedge-breaker after 16K consecutive refill
failures, (b) a rate-limited `needs_gc()` consult per refill. Both call the
same `maybe_gc_forced` the long-standing probe-failure arm in
`jit_new_object` uses. Their only novelty is WHEN they fire: mid-drain,
while young's free list is rich with split remnants and fragmentation-floor
mini-TLABs (4080-byte carves) are interleaved with live objects.

## Evidence read

The corrupt-run hex dumps around the "implausible object size" break point
decode as: [8 zero bytes][valid GAP_FILLER sentinel 0xF111E701, len=8]
[record with class_id=0 but NON-zero identity_hash (0x188) and num_slots
(e.g. 23579)] — i.e. the walk correctly skips a gap sentinel and then hits
a half-formed header: zeroed class_id with surviving hash/num_slots. That
shape suggests either a sweep-zeroed object whose neighbors' extents
disagree with the free list, or a header torn by an overlapping
free-block/carve accounting bug. The "cursor overshot into free block by
34..794 bytes" WARNs show an earlier stride was mis-sized — everything
walked since the previous anchor is suspect, and the mark phase's
conservative-candidate oracle walk breaks early at the same point, dropping
every stack root above the break (hence live subtrees swept → checksum
under-count, the historical 676xxxxx family).

## Investigation leads

1. Interaction of `refill_tlab`'s fragmentation fallback (largest-block
   carve at `frag_tlab_floor`) with `Arena::alloc`'s split bookkeeping —
   verify carve boundaries land exactly on free-block splits.
2. `install_tail_filler`'s GAP sentinel vs the sweep's gap decode when the
   gap abuts a swept-and-zeroed record.
3. Whether `sweep_young_non_moving`'s final reclaim pass can free a range
   overlapping a just-carved mini-TLAB when the collection was initiated
   from inside `tlab_alloc_object_inner` (the outgoing TLAB is retired
   before `maybe_gc_forced`, but the refill request is in flight).
4. `[quiesce] depth=15` — whether GC initiation from that JIT-helper depth
   leaves any per-frame root band unscanned (the OSR frame of
   `binaryTrees` + recursive `bottomUpTree` frames hold the only refs to
   the in-construction subtree).

## Why it matters

Without the triggers, bt18-style allocation-heavy workloads stay wedged in
the per-object slow path at -Xmx8g (~21s vs ~10.5s with triggers) — the
treadmill fix is blocked on this defect. And the defect itself is latent:
anything else that initiates a young collection mid-drain (multi-threaded
allocation storms, native-boundary GC-and-retry) can presumably hit it.
