# Proposal: trigger young collections on allocation since the last cycle (eden semantics), not on absolute occupancy

*Filed 2026-09-28 by gcd d9/b (lane ladder9). Not built.*

- **Kind:** proposal (policy; would replace three special cases).
- **Owner:** lane b (`gc/src/gen_heap.rs`, the young-trigger region).

## Why

The Generational young trigger fires on ABSOLUTE live occupancy of the
from-space (`live = used - free_list >= threshold`), with a post-collection
floor bolted on for each corner where that re-fires uselessly:

1. the anti-livelock floor (`live + capacity/16` when a cycle left `live` at
   or above the trigger, spring-webflux 2026);
2. the refusal floor (`note_skipped_young_cycle`, gengc-round1-core);
3. the wedged deferral at the 90 % cap (gen r4w4/oom);
4. gcd d9/b's raise below the cap on a wedged old generation, and gcd d9/c's
   blocked drain folded into both (`young_trigger_floor_judged`).

Each corner is the same fact seen from a different side: a collection is
worth running when the mutators have ALLOCATED enough since the previous one
for the collection to reclaim something, and a young set that could not be
drained (promotion blocked, old generation full, pinned) makes absolute
occupancy a bad proxy for that. HotSpot's Serial trigger is exactly this:
eden fills (allocation since the last young GC), whatever survived.

## Proposal

Keep one per-heap mark, `young_live_after_last_cycle` (sampled once per
completed collection, as the floor is today), and fire when

```
live - young_live_after_last_cycle >= budget
```

with `budget = threshold - min(young_live_after_last_cycle, threshold)`
floored at `capacity/16` -- i.e. "the nursery the pause-goal loop sized, minus
what is already occupied, but never less than a sixteenth" -- plus the
existing 90 % cap as a hard ceiling (allocation failure beyond it). On a
young generation that drains (the common case) `young_live_after_last_cycle`
is small and the rule is today's threshold; on one that cannot drain, each
cycle is guaranteed `capacity/16` of new allocation to reclaim, and the 90 %
cap / allocation failure takes over near the top.

This subsumes floors 1, 3 and 4 (and d9/c's term) without an old-generation
lock on the trigger path; floor 2 (a refused cycle) stays, since a refusal
does not complete a cycle.

## Risks

- A workload whose young survivors DO drain but slowly (long tenuring) would
  collect later than today while `young_live_after_last_cycle` is high; the
  pause-goal loop's input (survivor bytes) is unchanged, but its lever (the
  threshold) now means "allocation since" rather than "occupancy".
- `CRATONVM_DBG_GC_STRESS` has its own floor logic (`gc_stress_decision`)
  that would need the same restatement.

## How to verify

A/B behind an opt-in switch, interleaved, on `GenR4W5ThreadsOomProbe
-Xmx128m`, `GenR4W4HeapFullThrashProbe -Xmx128m`, `GenR4W3OomProbe`,
`GenR4W4EvacThroughputProbe -Xmx256m` (young cycle count against HotSpot's)
and one throughput benchmark (binarytrees-18): the OOME probes must keep
HotSpot's lines and their d9 pass rates, and the young cycle count must not
rise on the throughput rows.
