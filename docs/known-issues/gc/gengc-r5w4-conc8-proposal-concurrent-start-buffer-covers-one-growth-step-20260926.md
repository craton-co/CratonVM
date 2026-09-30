# Proposal: the concurrent start's buffer should cover one growth step, not only growth during a cycle

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 15
> of 54).** Not built (`CRATONVM_GC_CONC_START_STEP_BUFFER` absent). Its
> `start_owed` half is the third view of the owed-cycle mechanism, designed on
> the conc6 page. d7 caution for its verify step: the class-unload rows
> without `CRATONVM_GEN_YOUNG_MIRROR_DEFER` never unload whatever the cadence
> (u7_conc_w5: 63 concurrent cycles, `concunload_classes=0`), so the start
> buffer must be judged on `[GC] major_cadence:` and
> `concpol_cycles_completed`, not on `dead-loader-unloaded`. **Gate:**
> `GenR5W3ConcUnloadProbe` without `CRATONVM_GC_CONC_START_PERCENT`: 0 STW
> majors in the tail with the flag; `GenR4W5MajorCadenceProbe` and
> `GenR4W4SteadyPromotionProbe` same checksum and no more STW majors; the pure
> `threshold` unit test. **Size:** S.

*Filed 2026-09-26 by gen round 5 wave 4, lane `conc8`. Proposal (policy). Found
while working out the arithmetic of `GenR5W3ConcUnloadProbe`.*

## Problem

Once a cycle has been measured, the adaptive initiating occupancy is
(`ConcurrentStartPolicy::threshold`, `gc/src/concurrent_mark.rs`):

```
T = clamp(F − (5/4·G + C/32), 20 %·C, F)        F = 75 %·C (the STW floor)
```

Here `G` is the old-generation growth DURING a cycle. With no service thread
(the default), the cycle runs inline on the allocating thread, so a
single-threaded program's `G` is about 0 and `T ≈ F − C/32 ≈ 72 %`. The start
then sits 3 % of the generation below the stop-the-world floor.

The trigger is only asked at a young collection's epilogue, and between two
asks the old generation grows by one STEP:

- the promotion of one young collection, or
- any number of direct (humongous) old allocations, which ask nothing unless a
  service thread is attached.

If the step is larger than `F − T` (4 MiB at a 128 MiB old generation), the
generation jumps from below `T` to above `F` between two asks. The next young
pause's Phase 5 then runs a stop-the-world major, since no cycle is open, and
the concurrent cycle never gets its turn.

`GenR5W3ConcUnloadProbe` shows it by construction: one 34 MiB ballast per
round. The first cycle opens at 45 % (no measurement yet). Every later old
collection is a STW major unless `CRATONVM_GC_CONC_START_PERCENT` pins the
start. Any program that promotes more than about 3 % of the old generation per
young collection (a batch job, a cache fill) sees the same thing: the
concurrent-first policy degrades to "STW major every time" after its first
cycle.

## Proposal

Add the largest recent step to the buffer:

```
T = clamp(F − (5/4·G + max(C/32, S)), 20 %·C, F)
```

`S` is an EWMA (α = ½, like `G`) of the old-generation growth between two
consecutive trigger asks. It is measured where the trigger is asked
(`ConcurrentGcState::concurrent_start_due`): the difference of
`old_gen_allocated_total` between this ask and the previous one. That needs one
more `u64` in `StartPolicyState` and one subtraction per ask, with no new lock
(the policy lock is already taken there).

Alternatively (or also), let a humongous old allocation ask the trigger
without a service thread: set a per-VM `start_owed` bit that the next young
epilogue honours even if the occupancy has meanwhile crossed `F`. This is the
cheaper half, but it does not help a promotion-heavy young collection.

Behind an opt-in flag (`CRATONVM_GC_CONC_START_STEP_BUFFER`), since it moves
when cycles start.

## Cost and risk

- One subtraction and one EWMA update per trigger ask.
- The start moves EARLIER for programs with large steps, which means more
  concurrent cycles and fewer STW majors. It never moves later.
- `S` is capped by the clamp at `F − 20 %·C`.

## How to verify

1. `GenR5W3ConcUnloadProbe` with `CRATONVM_GEN_CONC_CLASS_UNLOAD=1
   CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats`, WITHOUT
   `CRATONVM_GC_CONC_START_PERCENT`:
   - with the flag, `[GC] major_cadence:` shows 0 STW majors in the 4 tail
     rounds, `concpol_cycles_completed >= 3`, and
     `concunload_layouts_released >= 1`;
   - without the flag, the tail runs STW majors.
2. `GenR4W5MajorCadenceProbe` and `GenR4W4SteadyPromotionProbe`: same
   checksum, and no more STW majors than today.
3. Unit test: a pure test of `threshold` with `S > C/32` lowers `T` by
   `S − C/32`, and with `S <= C/32` leaves it unchanged.
