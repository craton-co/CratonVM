# A TLAB refill can carve a buffer smaller than the object that triggered it

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): Generational FIXED and measured; the page stays open ONLY for the G1 / ZGC `refill_tlab_at_least` edit (out of this round's scope).** (The d5/q block's "correction to the brief" bullet concerns a proposal, not this page; ignore it.) Generational: `gcd_d1d_tests::a_refill_that_cannot_hold_the_object_declines_without_carving` and `gen_r5w5_sizer9_tests` pass in the round's Windows suite, and `GenR4W4NativeStringOomProbe -Xmx64m --nojit` prints the probe's four lines 5/5 with the share sizer off (`nsoom_nojit_1..5`) and 5/5 with it on (`nsoom_nojit_share_1..5`). (Those rows ran without `gc-stats`, so `gen_tlab_carved_bytes` was not read; and the four lines are the probe's own verdict, not HotSpot's: `../../internal/gc/gcd-d8x-native-string-oom-probe-is-not-a-hotspot-oracle-FIXED-20260929.md`.) **Remaining:** in `gc/src/vm_heap.rs::refill_tlab_at_least`, route the G1 and ZGC arms through a `refill_tlab_at_least(requested, min_size)` of their own (the exact edit in the d1/d block); then retire, or move that edit to a G1 / ZGC page and retire this one.

> **STATUS (2026-09-28, gcd d5/q): Generational FIXED (gcd d1/d, re-verified
> by reading at `d916d1c40`) -- RETIRE after the test and probe in the d1/d
> block below; the G1 / ZGC half stays the exact remaining edit written
> there (out of this round's scope).** No code change this wave.
>
> * Every Generational arm is closed: the VM site's single refill caller is
>   `tlab_alloc_shaped_inner`, both of its refills pass `object_floor`
>   (`VmHeap::refill_tlab_at_least`); `requested >= object_floor` on all
>   four sizing arms; `GenerationalHeap::refill_tlab_at_least` declines when
>   `requested.min(available) & !7 < min_size`, never lets the stress cap cut
>   below `min_size`, and `refill_fragmentation_fallback` raises its floor to
>   `min_size`, so its `largest.min(actual_size) & !7` is at least `min_size`
>   (both operands are, and `min_size` is on the 8-byte grid). The only other
>   callers of the plain `refill_tlab` are unit tests.
> * A correction to the brief this wave carried: "item 1 already landed
>   (`Arena::decommit_unused_above`, gcd d1/d)" is about the PROPOSAL
>   `../../internal/gc/gengc-r5w5-sizer9-proposal-floor-aware-young-decommit-DONE-20260928.md`
>   (gcd d4/p's survey), not this page.
>
> *Previous status (kept for the record):*

> **STATUS (2026-09-27, gcd d1/d): FIXED on Generational (clamp 2 landed,
> unbuilt when written); the G1 / ZGC half is an exact remaining edit below.
> Retire after the test and probe below pass.**
>
> **What landed.**
> * `GenerationalHeap::refill_tlab_at_least(requested, min_size)`
>   (`gc/src/gen_heap.rs`; `refill_tlab` is now `refill_tlab_at_least(_, 0)`,
>   byte for byte): declines BEFORE carving when `requested.min(available)`
>   is below `min_size` (young holds fewer free bytes than the object), and
>   `refill_fragmentation_fallback` takes `min_size` as a raised floor, so it
>   never carves a span the object cannot use. The debug stress cap
>   (`CRATONVM_DBG_GC_STRESS`) no longer cuts a request below `min_size`.
> * `VmHeap::refill_tlab_at_least` (`gc/src/vm_heap.rs`): Generational
>   forwards the floor; G1 and ZGC call their plain `refill_tlab` (unchanged
>   behaviour; a sub-object chunk there still fails the object as before).
> * `tlab_alloc_shaped_inner` (`vm/src/runtime/interpreter/gc_and_alloc.rs`)
>   passes `object_floor` (the missed object's footprint on the 8-byte grid)
>   at both refill calls (first try and the wedge-breaker's retry).
>
> **Effect on the default path.** `requested >= object_floor` already
> (sizer10), so a refill that could hold the object is unchanged. Only a
> refill that could NOT hold it changes: it used to carve and install a
> useless buffer (memset included) and return `None`; now it returns `None`
> without the carve, and the span stays in young's bump tail / free list for
> the slow path's smaller objects. One visible side effect on the JIT path
> (`refill_needs_young_room`): such a refill now counts toward the wedge
> breaker (`tlab_refill_wedge_break`, 16 384 consecutive failures, re-armed
> per 64 MiB) where the useless carve used to reset the counter — i.e. a
> young generation that cannot hold even the missed object is now treated as
> the wedge it is. Program output is unchanged.
>
> **Remaining (not this round's files):** G1's and ZGC's `refill_tlab` take
> the same floor. Exact edit: in `gc/src/vm_heap.rs::refill_tlab_at_least`,
> replace `VmHeap::G1(h) => h.refill_tlab(requested_size)` with
> `VmHeap::G1(h) => h.refill_tlab_at_least(requested_size, min_size)` (and the
> ZGC arm likewise), after adding to `G1Heap` / `ZgcRealHeap` a
> `refill_tlab_at_least` whose carve returns `None` when the chunk it would
> hand out is below `min_size`.
>
> **Test:** `cargo test -j 5 -p cratonvm-gc --lib -- gcd_d1d_tests::a_refill_that_cannot_hold_the_object_declines_without_carving`
> → passes (an 8 KiB bump tail: `refill_tlab_at_least(64 KiB, 20 KiB)` is
> `None` with `used` unchanged; `(64 KiB, 4 KiB)` is served from the tail;
> then `refill_tlab` finds young full). Also
> `cargo test -j 5 -p cratonvm-gc --lib gen_r5w5_sizer9_tests` (unchanged
> expectations).
>
> **Probe:** `GenR4W4NativeStringOomProbe -Xmx64m --nojit`, with
> `CRATONVM_TLAB_SHARE_SIZER=0` and `=1`: unchanged lines
> `fill: OutOfMemoryError "Java heap space"`, `native-strings ok`,
> `recovered ok`, `PASS` (5/5 each; the page's own expected rate). On the
> same runs `[GC] gen-alloc: tlab_refill_attempts=` minus `tlab_refill_ok=`
> may rise (declined carves are now counted as failed refills) while
> `gen_tlab_carved_bytes=` does not rise.
>
> *Previous status (2026-09-27, gen r5w6/sizer10), kept for the record:*
> **arm 1 FIXED (unbuilt); clamp 2
> OPEN.**
>
> **What landed.** `tlab_alloc_shaped_inner` now takes
> `object_floor = (total_size + 7) & !7` on EVERY arm: share sizer,
> `CRATONVM_TLAB_SIZE_RETIRED`, first refill and ladder. A request already at
> least the object is unchanged, so a run whose objects fit the ladder's
> answer carves byte for byte as before. Only a thread the ladder shrank
> below a missed object (8 or 16 KiB against an object of up to 32 KiB) now
> carves the object's size instead of a useless buffer.
>
> **Still open.** `refill_tlab`'s clamp to what young has left
> (`requested.min(available)`, the fragmentation fallback) can still hand
> out less than the object. That needs a minimum-size parameter on
> `VmHeap::refill_tlab`, which touches the G1 and ZGC signatures.
>
> **Verify.** `GenR4W4NativeStringOomProbe -Xmx64m --nojit`, with
> `CRATONVM_TLAB_SHARE_SIZER=0` and `=1`, must print unchanged lines:
> `fill: OutOfMemoryError "Java heap space"`, `native-strings ok`,
> `recovered ok`, `PASS`. On a run with small ladder buffers, expect no rise
> in `[GC] tlab-waste: retires=`.

*Filed 2026-09-27 by gen round 5, wave 5, lane `sizer9`, from reading
(adversarial review of `tlab_alloc_shaped_inner`). Not reproduced.*

- **Status:** OPEN (the share-sizer arm is fixed; the other arms are not).
- **Severity:** LOW–MEDIUM. Wasted carve and a spurious "young is exhausted"
  answer; on the native allocator that answer turns into old-generation
  batch spills (premature tenuring of short-lived native objects), which is
  the retention shape OOME-recovery probes trip on.
- **Backend:** every backend's VM refill site; the arena side is
  Generational (`GenerationalHeap::refill_tlab`).

## What is wrong

`vm/src/runtime/interpreter/gc_and_alloc.rs::tlab_alloc_shaped_inner` misses
on an object of `total_size` bytes (up to `tlab_max_alloc()` = 32 KiB), sizes
the refill (`requested`), retires the buffer, carves a new one and then tries
the object in it. Nothing makes the new buffer at least `total_size`:

1. **The ladder arms.** `Tlab::next_refill_size` / `refill_request_size`
   answer in `[min_tlab_size(), max_tlab_size()]` = 8 KiB .. 1 MiB; a thread the
   pressure tracker shrank to 8 KiB that misses on a 20 KiB array carves 8 KiB.
2. **`refill_tlab`'s own clamps.** `actual_size = requested.min(available)`
   (`available` = bump tail + free-list bytes), and the fragmentation fallback
   (`refill_fragmentation_fallback`) serves `largest.min(actual_size)` down to
   `frag_tlab_floor()` = 256 bytes; the stress cap (`CRATONVM_DBG_GC_STRESS`)
   caps the request at about `t / 8`.
3. (Fixed in r5w5/sizer9: the share-sizer arm now asks
   `.max(total_size rounded to 8)`.)

When the carve is smaller than the object, `alloc_initialized` fails on the
fresh buffer and the function returns `None` with an UNUSED buffer installed.
The interpreter's array path then allocates the object on the slow path
(harmless but wasted), and the native allocator
(`NativeContextImpl::native_alloc_no_gc_arms`, `vm/src/vm/vm_exec.rs`) reads
the `None` as "young could not supply another TLAB" and switches to
`try_alloc_objects_old_batch` — 2048-object old-generation batches of that
layout, kept in a rooted per-thread pool.

## Proposed fix (S)

In `tlab_alloc_shaped_inner`, take `requested.max((total_size + 7) & !7)` on
every arm (one line beside the sizer arm's). For clamp 2, make the refill's
contract "at least `min_size` or nothing": pass `total_size` to
`VmHeap::refill_tlab` as a minimum, have `GenerationalHeap::refill_tlab`
decline (return `None` without carving) when neither the bump tail nor the
fragmentation fallback can reach it, and leave the caller's current `None`
handling unchanged. G1's and ZGC's `refill_tlab` would take the same
parameter (out of this round's scope: their files).

Why not landed here: arm 1 changes TLAB sizing on the default path (policy,
needs its A/B), and clamp 2 changes `VmHeap::refill_tlab`'s signature across
all three backends.

## How to verify

A unit test on a 1 MiB young heap: fill young to leave an 8 KiB bump tail and
no free block, then `tlab_alloc_array` a 20 KiB array — today it returns
`None` and leaves an 8 KiB buffer; after the fix it returns `None` WITHOUT
carving (or `Some` when a 20 KiB span exists). Runtime: `[GC] tlab-waste:`
`retires=` and `carved=` fall on `GenR4W4NativeStringOomProbe -Xmx64m --nojit`
with no change in its lines (`fill: OutOfMemoryError "Java heap space"`,
`native-strings ok`, `recovered ok`, `PASS`).
