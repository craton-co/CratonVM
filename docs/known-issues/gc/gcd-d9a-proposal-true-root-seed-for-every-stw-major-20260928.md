# Proposal: the true-root young seed for every stop-the-world major, and step 1 on by default

> **STATUS (2026-09-28, gcd d9/a): PROPOSAL, not built.** Follows
> `gcd-d5r-proposal-true-root-seed-beyond-requested-non-promoting-majors-20260928.md`,
> whose steps 1 and 2 gcd d9/a built.

*Filed 2026-09-28 by gcd d9/a (trueroot9).*

## Where the true-root seed stands after gcd d9/a

| Major | Young seed |
|---|---|
| requested (`System.gc()`, every allocation-ladder major), non-moving cycle | true roots, promoting cycles and planned pinned compactions included (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, default on) |
| requested, moving cycle (`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`) | legacy (counted: `oldsz_true_root_fb_moving_cycle`) |
| occupancy-triggered, non-moving cycle | legacy (not counted: never armed) |
| occupancy-triggered, moving cycle (Phase 5) | legacy |
| concurrent cycle | its own seed (step 3 of the d5r page) |

## 1. Arm the seed on every non-moving major

`run_non_moving_young_cycle` arms `TrueRootMajorScope` only for
`major_requested`. Nothing in the seed depends on the request: the only
reason it was scoped so was risk. An occupancy major under the legacy seed
keeps a dead old holder's young child's old chain for one more major (floating
garbage), which on a JIT-warm program (every cycle non-moving) is every major.
Gate: arm it under an opt-in `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS`,
measure `[GC] oldgen_sizing:` `oldsz_true_root_young_excluded` and the major
pause lines (`door=`) on `GenR4W4SteadyPromotionProbe`, the thrash and
ThreadsOom rows, and the battery; flip when no row regresses.

## 2. The moving Phase 5 major

After the Cheney copy, every young survivor is in the post-swap from-space and
`pointer_map` holds `young -> copy` and `young -> old` for all of them; the
roots are already rewritten. A true-root seed there is `TrueRootYoung` over
the post-swap grid (no promotion table needed: roots and side tables that the
collector rewrote are current; the rows keyed by pre-copy addresses need the
same source-keyed lookups as the non-moving path, through `pointer_map`).
Fix `gcd-d9a-moving-major-misses-side-table-rows-of-moved-owners-FIXED-20260929.md`
first: it is the same lookup.

## 3. Step 1 by default

`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG` (opt-in) reclaims the excluded
young survivors in the same pause. It is what lets ONE requested major free a
dropped chain that alternates old/young, i.e. what makes the allocation
ladder's second major (`majors_to_decide_oome`) unnecessary. Flip gate:
`CRATONVM_GC_OVERHEAD_PROGRESS=0 CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1
... GenR4W4NativeStringOomProbe -Xmx64m` prints its four lines 5/5; the
battery with the flag forced on shows no new DIFF row and no crash (a young
object freed under a live reference faults as a zero header); the gc crate's
unit suite passes with it forced on. Then consider retiring the second major.
