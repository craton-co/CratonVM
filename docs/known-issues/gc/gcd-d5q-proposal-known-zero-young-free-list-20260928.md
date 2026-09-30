# Proposal: the young arena remembers that its free list is zero, so a swept hand-out is zeroed once

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 14
> of 54).** Not built (`add_zeroed_free_block`,
> `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` absent). Item 1 of
> `gengc-r5w6-pin10-proposal-young-copy-round-three-20260927.md` in full
> detail; kept as its own page because the door table is the design. The
> licence it needs already holds on d7: z2_smallalloc
> (`CRATONVM_DBG_DEADREF_STORE=1 GenR4W4SmallAllocProbe 4 20000000`) reports
> `filler_over_object=0 refill_over_object=0 gen_zero_repairs=0` on the
> default arm. **Gate:** the page's licence run with the switch on, then
> `gen_zero_skipped_bytes` up by about the swept bytes handed out and
> `time_ms` lower, interleaved. **Size:** S-M (arena fact + two call sites in
> the sweep).

*Filed 2026-09-28 by gcd d5/q (lane alloc5), from reading `d916d1c40`. Not
implemented; no run. Item 2 of
`gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`, and the
last double write of
`gengc-r4-alloc-tlab-memory-is-zeroed-twice-FIXED-20260929.md`.*

- **Status:** PROPOSAL (perf, opt-in first).
- **Backend:** Generational.
- **Code:** `gc/src/arena.rs` (lane alloc), `gc/src/gen_heap.rs`
  `zero_young_hand_out` / `refill_tlab_at_least` / `try_alloc_young_initialized`
  (lane alloc) and the publication loop of `sweep_young_non_moving` (lane
  young).

## What is wasted

On a JIT-warm program every young cycle is the in-place sweep. Its
publication step zeroes every reclaimed span (`zero_spans_parallel`, so a
conservative scan never meets a stale header in a hole) and lists it
(`Arena::add_free_block`). The next TLAB refill from that free list zeroes
the span AGAIN (`GenerationalHeap::zero_young_hand_out`: "a free-list
hand-out ... its zeroing point is here"). The zero-once skip
(`CRATONVM_GEN_ZERO_ONCE`, default on) covers only bump-tail hand-outs,
because the arena carries no fact about what a listed block holds. On
`GenR4W4EvacThroughputProbe` that is most of each cycle's allocation.

## Proposed change

One arena-level fact, not a per-block bit (a `FreeBlock` flows through the
small-bucket tier, the span tier, splits, merges, `coalesce_free_list` and
ZGC's rebuilds; a bit would have to be threaded through all of them):

`Arena::low_free_list_zero: bool` -- "every block on the LOW free list reads
zero right now".

| Door | Effect |
|---|---|
| `new`, `reset`, `reset_deferring_zero`, `clear_free_list`, `clear_low_free_list` | true (empty list) |
| `add_free_block` (low region) | **false** (content unknown) |
| new `add_zeroed_free_block` | unchanged (caller proves zero) |
| `return_unused_tail` (`FreeListed`) | unchanged (an unused TLAB tail, zero since its carve) |
| `adopt_bump_alignment_padding` | unchanged (bytes above the cursor) |
| split remnants in `alloc` / `small_fit` / `large_fit` | unchanged (part of a listed block) |
| `coalesce_free_list`, `retract_cursor_into_free_tail` | save and restore (they re-add blocks already listed) |
| `rebuild_low_free_list`, `rebuild_after_in_place_evacuation`, `compact_low_to` | false (the pinned in-place cycle POISONS quarantined spans; ZGC's rebuild is not audited) |

Consumers:

- `sweep_young_non_moving` (lane young): the publication loop after
  `zero_spans_parallel` and the coalesce re-add call `add_zeroed_free_block`.
  Without these two call sites the fact is always false and nothing changes.
- `refill_tlab_at_least` / `try_alloc_young_initialized` (lane alloc): a
  hand-out that is not `hand_out_is_bump_tail` but was served while
  `low_free_list_zero` held takes the bump-tail arm of `zero_young_hand_out`
  (debug-build sample check, `young_zero_repairs` on a non-zero word,
  `CRATONVM_DBG_DEADREF_STORE` whole-span scan).

The hazard is the one the bump-tail skip already carries: a stale write into
a listed block (a dangling reference) would be handed out instead of
repaired. The O(1) tripwire word and the debug sample keep the same
detection. Opt-in (`CRATONVM_GEN_ZERO_ONCE_FREE_LIST`), then flip after the
licence run below.

## How to verify

- Licence: `CRATONVM_DBG_DEADREF_STORE=1` with the switch on,
  `GenR4W4EvacThroughputProbe -Xmx256m` and `BinT 14 -Xmx256m`: the probe's
  `PASS ... corrupt=0` line, `[GC] tlab-guard: filler_over_object=0
  refill_over_object=0`, `[GC] gen-alloc: gen_zero_repairs=0`.
- Effect: `gen_zero_skipped_bytes=` rises by about the swept bytes handed out;
  `time_ms` A/B interleaved.
