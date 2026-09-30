# JIT round 14 lane monitor3: proposals (contended enter)

Status: OPEN (proposal book; ideas, not work items)
Area: `vm/src/threading/monitor.rs`, `vm/src/vm/vm_exec.rs` (`monitor_enter_blocking`, `enter_synchronized_method_blocking`-style callers of `spin_try_enter`)
Severity: proposals
Found by: round 14 wave 6 lane monitor3

Ranked by expected benefit per unit of risk. None was measured (no builds, no runs); each names
the census row (`CRATONVM_DBG_MONITOR_CONTENTION`, round 14 wave 6 additions) that decides it.

## MC3-1. One abort per contended enter, not two

**What.** Under `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT=1` (with the lazy park), an adaptive spin
that aborts at a hand-over returns `Some(monitor)` from `MonitorTable::enter_or_contend`, and
`vm_exec::monitor_enter_blocking` (and its synchronized-method twin, ~5950) then runs
`Monitor::spin_try_enter`, which spins again until ITS first observed hand-over. Have
`enter_or_contend` report why it contends (`Contended { spun_out, aborted }`), or record the abort
on the `JvmThread` for the call, and skip the fixed spin after an abort (go straight to the lazy
park). **Benefit.** The crowd case the arm is for: one spin, then the cheap park.
**Cost.** Small (one field or one return shape; two callers in `vm_exec.rs`, an interpreter-round
file). **Risk.** Low (skipping a spin is always sound: the caller parks as after a failed one).
**First step.** The census run on the monitor page: if `spin_handover_aborts` is dominated by
`spin_try_enter`'s share (both spins count there) the second spin is waste.

## MC3-2. Pick the default lazy-park budget from the entrant-wait histogram

**What.** `CRATONVM_MONITOR_LAZY_PARK_US` is opt-in with a free-form budget. The new
`entrant_waits_under_100us` / `_under_1ms` / `_under_16ms` / `_longer` rows say how long GC-blocked
parks actually last; default the budget to the bucket edge that covers ~90% of waits on
`ThreadChurn`, `R11W15LockLeaseChurn`, `R14Monitor3Crowd` (never above one Windows tick unless
the `_under_16ms` bucket dominates, since the timed wait rounds up to the tick there).
**Benefit.** Removes the GC-blocked protocol (TLAB retire, root deposit, three barrier-lock
acquisitions) from most parks -- item 4 of the monitor page without touching `gc_barrier.rs`.
**Risk.** Medium (a pause waits up to the budget for a running parker; the initiators already
wake them). **First step.** The census run.

## MC3-3. A per-monitor "parks are short" bit

**What.** Once MC3-2 has a default, let each monitor learn it: an exponentially weighted entrant
wait (updated under the state lock the parker already holds at acquisition) that, when short,
sends a contender to the lazy park even before its spins (HotSpot's `_SpinDuration` for the park
side). **Benefit.** Workloads whose critical sections are long but hand-overs frequent (the
`long-8t` phase). **Cost.** Medium. **Risk.** Medium (a new adaptive policy; must keep the
lost-wake-up argument, which it does if it only chooses between existing paths).
**First step.** MC3-2.

## MC3-4. Count the inline spin's hand-overs too

**What.** The round-14-wave-1 inline census words count the compiled spin's outcomes, not whether
it watched a hand-over. On sites built under `CRATONVM_DBG_JITC`, a fourth word bumped on the lost
`LOCK CMPXCHG` edge (RCX is the monitor there, as for the three existing bumps) gives the compiled
share of `spin_wins_after_handover`. **Benefit.** Completes the M2-2 census for compiled code,
where most contended enters start. **Cost.** Small (`runtime_lowering.rs`, one offset pinned both
sides on the entry count's line, which has 32 spare bytes). **Risk.** None with the census off
(byte-identical code). **First step.** The offset constant in `runtime_lowering.rs`.
