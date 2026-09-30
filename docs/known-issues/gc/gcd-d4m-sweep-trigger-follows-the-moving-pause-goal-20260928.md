# On a JIT-warm program the sweep runs at the MOVING trigger, so young collects 4-5x as often as HotSpot

> **STATUS (2026-09-29, gce e1/x): KEEP -- item 2's consumer landed opt-in (`CRATONVM_GEN_ZERO_ONCE_FREE_LIST`, e1/y requests 1-3 applied); its licence rows ran but were not judged.** `zeroZ_fl_1..3` end rc 0 (`verify-e1/ve1`); the known-zero counters are not in the retained output. Item 1 (`CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`) has no row. **Remaining:** the licence counters and repairs from those rows' stderr, the ABBA of the page's e1/y block, and item 1's three-arm A/B.

> **STATUS (2026-09-29, gce e1/y): item 2 WRITTEN (producer and arena fact landed; the one-line consumer in the refill is a cross-lane request to lane o, opt-in); item 1 unchanged (opt-in, unmeasured).** Unbuilt when written.
>
> - **Design (a span set, not a list-wide bit).** d5/q's "every block on the low free list reads zero" fact dies for good the first time any unproven block is listed (a TLAB tail, a true-root young free), and nothing re-establishes it on a sweep-only program. Instead the arena keeps the SPANS the last sweep proved zero: `Arena::known_zero` (`gc/src/arena.rs`), installed by `set_known_zero_free_spans`, asked by `hand_out_is_known_zero(off, len)` (the hand-out lies wholly inside one span; O(log spans)), and `known_zero_bytes` (census). The claim is about bytes, so the free list's splits and merges cannot falsify it; bytes stop being zero only once handed out, and handed-out bytes are re-listed only through doors that FORGET the set: `add_free_block`, `return_unused_tail` (both arms), `clear_free_list` (every reset, the sweep's own coalesce), `clear_low_free_list` (`rebuild_low_free_list`, `rebuild_after_in_place_evacuation` -- which also poisons quarantined spans -- and `compact_low_to`), `retract_cursor_to`, `retract_cursor_into_free_tail`, `grow`. `coalesce_free_list` (the alloc last-resort merge) saves and restores it; `alloc`'s split remnants keep it. The next sweep replaces it.
> - **Producer (young lane, landed):** `sweep_young_non_moving` installs `reclaimed_regions` -- exactly the spans `zero_spans_parallel_on` zeroed and the publication loop listed -- right after `arena-coalesce` (whose `clear_free_list` + re-adds forget any earlier set), and only when reclamation was not deferred. Nothing between the publication and there writes a reclaimed span (the mark-clear re-walk is hole-aware). Default behaviour is unchanged: nothing reads the set until the consumer is on.
> - **Consumer (lane o's refill region, cross-lane request 1 of `docs/internal/gc-design-perf-round-20260929/e1-y-report.md`):** in `refill_tlab_at_least` and `try_alloc_young_initialized`, under the young lock, `known_zero_reuse = !from_bump && gc_flags().gen_zero_once_free_list && from.hand_out_is_known_zero(off, len)`, and the hand-out takes the bump-tail arm of `zero_young_hand_out` (`bump_tail = zero_once && (from_bump || known_zero_reuse)`): the same debug sample, tripwire and `young_zero_repairs` repair. New switch `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` (opt-in, `non_empty_non_zero`).
> - **Tests:** `cargo test -j 5 -p cratonvm-gc --lib e1y_` (arena: `e1y_a_hand_out_is_known_zero_only_wholly_inside_one_span`, `e1y_doors_that_relist_handed_out_bytes_forget_the_set`, `e1y_a_merge_and_a_split_keep_the_set`; sweep: `e1y_a_sweep_publishes_the_spans_it_zeroed_as_known_zero`).
> - **Verify (once the consumer and the switch are in):** licence run, `CRATONVM_GEN_ZERO_ONCE_FREE_LIST=1 CRATONVM_DBG_DEADREF_STORE=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe` prints `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0` with `[GC] tlab-guard: filler_over_object=0 refill_over_object=0` and `gen_zero_repairs=0`; then ABBA without the DBG knobs: `gen_zero_skipped_bytes` rises by about the swept bytes handed out, `gen_zeroed_reuse_bytes` (lane o's census) falls by the same, `time_ms` not worse. Also `BinT 14 -Xmx256m` under the licence knobs.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7): unchanged -- item 1 opt-in (`CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`, default OFF), item 2 unwritten.** No d7 row runs the opt-in arm, so nothing was measured for it this wave; the default is byte-for-byte as before. Remaining: the item-1 A/B on a JIT-warm probe (young cycle count against HotSpot's) and the free-block known-zero design of item 2 (`gcd-d5q-proposal-known-zero-young-free-list-20260928.md`).

> **STATUS (2026-09-28, gcd d5/q): item 1 FIX LANDED behind the opt-in
> `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` (default OFF, so the default is
> byte-for-byte unchanged); item 2 NOT implemented (design narrowed below,
> needs lane s). Unbuilt when written.**
>
> *Item 1, what landed (`gc/src/gen_heap.rs`):*
>
> * `sweep_own_trigger_bytes(capacity, adapted)`: the sweep arm's trigger,
>   its own adapted value clamped to `[capacity/16, 90 %]`, the 90 %
>   non-moving trigger until the loop first moves it.
> * `GenerationalHeap::needs_gc_with_jit_allocation_frame`: when the next
>   cycle is predicted to sweep (`non_moving_young`) and the heap latched the
>   flag, the threshold is `sweep_own_trigger_bytes(..)` instead of
>   `young_gc_trigger_bytes(..)`; otherwise the old expression.
> * `adapt_young_trigger_to_pause` → new `adapt_sweep_trigger_to_pause`: a
>   SWEEP's pause (stamp `non_moving_latch[0] == minor_gc_count`) feeds a
>   separate `TriggerFeedback` (`young_sweep_feedback`) and threshold
>   (`young_sweep_threshold`) with the same law (`next_young_trigger`): a
>   halving that does not buy a quarter of the pause is reverted and latched,
>   which is the sweep's case (its walk covers the never-retracted cursor
>   whatever the trigger). The moving threshold and its pending trial are no
>   longer touched by a sweep's pause. A pause that also reclaimed old gen
>   still feeds nothing.
> * New struct fields `young_sweep_own_goal` (latched at construction),
>   `young_sweep_threshold`, `young_sweep_feedback`; new summary line
>   `[GC] young_sweep_trigger:` (printed only with the flag, keys `sweep_*`,
>   added to `vm_heap.rs`'s key-uniqueness corpus).
> * The doc of `young_gc_trigger_bytes` no longer calls "no goal" the default.
> * Flag declared in `types/src/flag_groups.rs` (`gen-sweep-trigger-own-goal`)
>   and `types/tests/flag-surface.txt`; the orchestrator regenerates
>   `docs/config/flag-inventory.md` and `docs/flag-tokens.md`.
>
> *Tests:* `cargo test -j 5 -p cratonvm-gc --lib gcd_d5q_sweep_trigger_tests`
> (3 tests: the band; a sweep pause halves then reverts only the sweep loop;
> off, the moving loop takes it as before) and
> `cargo test -j 5 -p cratonvm-gc --lib every_gc_summary_key_is_unique_across_the_whole_summary`.
>
> *A/B (Linux, idle host, interleaved A B B A, 4 rounds):*
>
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench"
> cratonvm $P GenR4W4EvacThroughputProbe                                            # A
> CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL=1 cratonvm $P GenR4W4EvacThroughputProbe      # B
> CRATONVM_GC_YOUNG_PAUSE_MS=0 cratonvm $P GenR4W4EvacThroughputProbe               # C (no goal at all, control)
> ```
>
> Every run prints `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`
> and a `time_ms=` line. B alone prints `[GC] young_sweep_trigger:`
> (`sweep_trigger_bytes=` near 90 % of `sweep_semi_capacity=` unless
> `sweep_adapt_halvings=` stuck). Accept B when its `[GC] generational:
> minor=` is well below A's (the page's 130-170; C shows the floor the goal
> costs) and its median `time_ms` is lower; B's `[GC] young_trigger:
> adapt_halvings=` should stay near 0 on this probe (no moving cycle). Also
> with the flag: `GenR4W4JitWarmDivertProbe` → `PASS jitwarm threads=4
> calls=1500000 checksum=-4668312146048759300`; `GenR4W6JitOomRootProbe
> -Xmx64m` and `GenR4W3OomProbe` → the same lines as the flag-off run of the
> same binary.
>
> *Item 2 (the second zeroing of swept bytes), not implemented.* A per-block
> bit is the wrong grain: `FreeBlock` flows through the small-bucket tier,
> the span tier, splits, merges, `coalesce_free_list` and ZGC's rebuilds. The
> narrower design is one ARENA fact, "every block on the LOW free list reads
> zero", in `gc/src/arena.rs` (this lane's): true when the list is empty
> (`reset`, `clear_free_list`); kept by `return_unused_tail` (an unused TLAB
> tail), `adopt_bump_alignment_padding` (bytes above the cursor), splits and
> `coalesce_free_list`/`retract_cursor_into_free_tail` (they re-add blocks
> already listed); cleared by a plain `add_free_block`,
> `rebuild_low_free_list`, `rebuild_after_in_place_evacuation` (the pinned
> in-place cycle poisons quarantined spans) and `compact_low_to`; kept by a
> new `add_zeroed_free_block` that the sweep's publication loop (after
> `zero_spans_parallel`, lane s's region of `sweep_young_non_moving`) and its
> coalesce re-add would call. `zero_young_hand_out` would then skip a
> free-list hand-out when the fact holds, with the bump-tail skip's sample
> check and repair counter. Opt-in, and it needs lane s's two call sites, so
> it is filed as `gcd-d5q-proposal-known-zero-young-free-list-20260928.md`.
>
> *Previous status (the filing), kept for the record:*

*Filed 2026-09-28 by gcd d4/m (lane young4), from reading `d3e80a4ec`. No
run. This is the largest cost term on `GenR4W4EvacThroughputProbe`'s default
path (see `gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`).*

- **Status:** OPEN (perf; a policy change, so opt-in first).
- **Backend:** Generational, default flags.
- **Code:** `gc/src/gen_heap.rs`: `young_gc_trigger_bytes` and its caller
  `needs_gc_with_jit_allocation_frame` (the trigger);
  `adapt_young_trigger_to_pause` (the pause goal); `zero_young_hand_out`
  (item 2).

## What is wrong

1. **The trigger.**
   - `young_gc_trigger_bytes(capacity, moving_threshold, non_moving_young,
     pause_goal_armed)` answers `moving_threshold` on the sweep arm too, when
     a pause goal is armed and the moving threshold is lower. The sweep arm's
     own trigger would be `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT` = 90 % of
     the semi-space.
   - `DEFAULT_YOUNG_PAUSE_GOAL_MS` = 200 arms the goal by default, and the
     sweep's pause grows with the allocated span (its serial whole-arena
     walks). So the goal keeps lowering the moving threshold, and the sweep
     runs at it: at most 50 % of the semi-space, less after adaptation.
   - The doc comment on `young_gc_trigger_bytes` still says "With no goal
     (the default) this is byte-for-byte the previous behaviour". But a goal
     IS the default.
   - On the probe that is about 130-170 young cycles against HotSpot Serial's
     ~34. Each one re-walks the retained ring and re-seeds every dirty card.
2. **The second zeroing.**
   - A free-list hand-out is zeroed by `zero_young_hand_out`, although the
     sweep already zeroed every reclaimed byte (`zero_spans_parallel` in its
     publication step).
   - The zero-once skip (`CRATONVM_GEN_ZERO_ONCE`) covers bump-tail
     hand-outs only, because the arena's free list carries no "known zero"
     fact.

## Proposed fix

1. Opt-in (e.g. `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`). On the sweep arm,
   adapt a SEPARATE threshold against the sweep's measured pause, bounded
   below by the non-moving floor and above by the 90 % trigger, instead of
   borrowing the Cheney arm's. Or simply ignore the goal on the sweep arm.
   Measure with `GenR4W4EvacThroughputProbe` (`time_ms` and the `[GC]`
   young cycle count) and `GenR4W4JitWarmDivertProbe`, both arms, ABBA. The
   OOM probes (`GenR4W6JitOomRootProbe`, `GenR4W3OomProbe`) must print the
   same lines.
2. The arena records, per free block, whether the sweep zeroed it (one bit
   beside `FreeBlock`). `zero_young_hand_out` then skips a hand-out wholly
   inside zeroed blocks, with the same debug-build sample check and repair
   counter as the bump-tail skip. This is `gc/src/arena.rs` plus
   `refill_tlab`; it is also a zeroing-invariant change, so opt-in first.

## How to verify

```
CRATONVM_DBG_YOUNG_TRIGGER=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
  -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
```

Today the `[young-trigger]` lines print `non_moving=true` with `threshold`
at or below half the semi-space. The fix must print
`PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`
with fewer young cycles in the `[GC]` summary.
