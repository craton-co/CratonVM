# TLAB-trigger GC exposes young-walk/free-list corruption (bt18 under-count)

Status: **FIXED 2026-07-18** (perf/halfgap-residuals-20260718).
`CRATONVM_TLAB_GC_TRIGGER` is now **default ON** (opt out with `=0`).

## Root cause (found via free-list alignment tripwires + backtrace)

The young arena's ergonomics-derived capacity was **not 8-aligned**
(observed live: 1 GiB - 4). `Arena::remaining()` therefore carried a
permanent mod-8 dreg, and when young ran nearly full — exactly the regime
the refill-time triggers create — `refill_tlab`'s
`actual_size = requested.min(available)` minted **unaligned TLAB sizes**
(32764, 9772, ... ≡ 4 mod 8). Two independent corruptions follow:

1. `Arena::alloc(actual_size)` served from a free-list block leaves a
   **split tail remnant at an offset ≡ 4 mod 8** — an off-grid free block.
   `skip_free_blocks` then resyncs the walk to that block's off-grid end,
   and every subsequent stride is suspect ("cursor overshot into free
   block", free blocks at +4 offsets breeding more +4 blocks).
2. `Tlab::new`'s release-mode safety net rounds an unaligned TLAB end
   **down**, leaving an untracked zeroed 4-byte sliver between the TLAB's
   tail filler and the next region — the "[GAP sentinel][unaccounted zero
   bytes][real object]" hex shape: the walk parses the zero run as a
   phantom object and desyncs off the grid.

The checksum under-count (676xxxxx family) was amplified by the mark
oracle: the candidates-only exact-base walk **silently dropped every
conservative root above an early break**, so whole live subtrees lost
their roots and were swept.

## Fix (four layers, gc/src/arena.rs + gc/src/gen_heap.rs)

1. `Arena::new`/`Arena::grow` round the capacity down to a multiple of 8
   (kills the unaligned bump-tail origin).
2. `Arena::alloc` rounds every allocation size **up** to a multiple of 8 —
   the free list and bump cursor can never leave the object grid again,
   regardless of caller. A bounded alignment tripwire
   (`warn_unaligned_block`) still names any caller that passes an
   unrounded size.
3. `refill_tlab` rounds `actual_size`/`take` **down** to a multiple of 8 so
   a TLAB's end never triggers `Tlab::new`'s round-down sliver.
4. The mark oracle records its trusted frontier; conservative candidates
   above a truncated walk fall back to direct plausibility-checked
   validation (pre-oracle behavior, over-retention-safe) instead of being
   silently unrooted.

## Validation

- bt18 default heap, triggers ON: **5/5 checksum 68332206** (was 5/5
  wrong), zero walk warnings.
- bt18 -Xmx8g triggers ON: 10.64 s stable ×3, correct checksum (the
  treadmill fix this trigger set was built for).
- StreamOnlyStressRepro -Xmx32m ×3 + -Xmx512m: `RESULT=OK` (mark-oracle
  regression face).
- `cargo test --release -p cratonvm-gc --lib`: 791/791.

Original OPEN report follows for the record.

---

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
