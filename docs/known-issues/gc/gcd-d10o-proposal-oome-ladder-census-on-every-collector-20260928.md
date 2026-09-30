# Proposal: the `[GC] oome_ladder:` census on every collector, and one ladder decision for all four doors

*Filed 2026-09-28 by gcd d10/o (lane oom10). A proposal; nothing is broken
by leaving it.*

## Where things stand

The allocation-failure ladder is implemented four times, once per door:

| door | where |
|---|---|
| interpreter (`new`, `newarray`, the String ladders, `multianewarray`, `native_alloc_collecting`) | `gc_and_alloc.rs::collect_and_retry_with_thread` |
| native factories (`reclaim_before_alloc_retry`) | `gc_and_alloc.rs::native_reclaim_before_alloc_retry` (gcd d10/o; the body in `vm_exec.rs` until lane f repoints it) |
| JIT helpers | `jit/helpers.rs`: `jit_latched_overhead_limit_throws`, `jit_overhead_limit_verdict`, `jit_g1_last_ditch_full_cycle` |
| exception objects | `exceptions.rs::create_exception_object_for_class_inner` |

Each round that fixed one of them had to hand-carry the fix to the others,
and several did not arrive: the native door never got gen r4w4's major or
gcd d4/j's second major (found by gcd d10/o), and G1's marking cycle was
missing from the latched exit of all four
(`gcd-d10o-g1-latched-overhead-exit-skips-the-marking-cycle-20260928.md`).
`gcd-d1b-proposal-one-allocation-ladder-for-jit-helpers-REJECTED-20260929.md`
proposes folding the JIT copy into the interpreter's; this page asks for the
general form plus the observability that would have caught the drift.

And the one census of the ladder, `[GC] oome_ladder:` (gcd d9/b), is
Generational-only: `ladder_census` writes into `GenerationalHeap`'s
counters and is a no-op on G1 and ZGC, so on those collectors nothing says
which rungs a failing run climbed (the `[GC_OVERHEAD]` debug lines are the
only witness, and they print per collection).

## Proposal

1. **One decision function.** A pure `LadderStep` state machine in
   `gc_and_alloc.rs` -- inputs: backend, latched, which collections this
   thread won, whether the last one was productive; outputs: the next rung
   (`Forced`, `SoftRung`, `DecidingMajors`, `G1Cycle`, `LastDitch`,
   `SecondMajor`, `Attempt`, `Throw`). Each door keeps its own way of
   ATTEMPTING (a closure, a native retry, a JIT retry) and of throwing, and
   asks the machine for the next rung. The pure function is unit-testable per
   backend, which none of the four copies is today (their tests are source
   witnesses or need a wedged heap).
2. **A census every backend fills.** Move the `oome_ladder` slots from
   `GenerationalHeap` to the per-VM `HeapRealm::alloc_ladder` (already per VM,
   already holds `oome_native_debt` and `jit_alloc_oom_without_thread`), keyed
   by rung and by door, and print the same `[GC] oome_ladder:` line at
   shutdown on every collector. A drift like the native door's then shows as
   a door whose rung counts differ from the others' on the same run.

## Cost and risk

Code motion in a file one lane owns plus lane f's JIT helpers; the behaviour
is meant to be identical, so the gate is the OOME battery (the d9 pass
rates of `GenR4W6JitOomRootProbe`, `GenR4W4HeapFullThrashProbe`,
`Gcd1PinnedCalleeOomeProbe`, `GenR4W4NativeStringOomProbe` on all three
collectors) before and after. The census alone (step 2) is independent and
cheap: relaxed adds on the failure path only.
