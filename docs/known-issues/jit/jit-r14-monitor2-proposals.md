# JIT round 14 lane monitor2: proposals (wait/notify after MON14-1 / MON14-2 / MON14-4)

Status: OPEN (proposal book; ideas, not work items)
Area: `vm/src/threading/monitor.rs` (`Monitor::wait`, `MonitorTable::wake_waiter_for_interrupt`), `vm/src/vm/vm_exec.rs` (`thread_interrupt`, `monitor_wait_with_state`), `vm/src/threading/thread_registry.rs` (`jmx_waiting_monitor`)
Severity: proposals
Found by: round 14 wave 4 lane monitor2

Ranked by expected benefit per unit of risk. Nothing here was built or measured by the lane.

## MW4-1. A census row for waits the safety slice ended with a flag set

**What.** Under `CRATONVM_MONITOR_WAIT_SINGLE_PARK` (MON14-4) the only thing standing between a wake-less
path and a stall is the 1 s safety slice, and nothing says when it fired for a reason. Count, in
`Monitor::wait`, a park that timed out (`result.timed_out()`), found no credit, and then found the
interrupt flag or the stack-dump flag set: that is an event whose wake was missed. A
`ContentionEvent::WaitSafetySliceCatch` row printed by the existing
`CRATONVM_DBG_MONITOR_CONTENTION` report (which is its production reader, as the orphan gate
wants). **Benefit.** Turns "a missed wake costs up to 1 s" from an argument into a number that
should read 0 on every census run; a non-zero row names a path to fix. **Cost / risk.** Small /
none (one branch on the cold timed-out edge, counted only under the census flag). **First step.**
Add the row next to `wait_reacquire_spin_wins` and run `R14Monitor2InterruptOne` under the census.

## MW4-2. Find the interrupted waiter's monitor without the object

**What.** `thread_interrupt` finds the target's monitor through the registry's
`jmx_waiting_monitor` (an `ObjectRef`, GC-forwarded), then the object's mark word and the index
shard lock (`lookup_indexed`). `Monitor::wait` already holds the `Arc<Monitor>`: publishing a
`Weak<Monitor>` (or the `Arc`) in the registry entry for the length of the wait -- written with
`jmx_waiting_monitor`, taken with it -- would let the interrupt signal the entry directly, with no
header read and no shard lock, and would work while the object's slot is being forwarded.
**Benefit.** A cheaper interrupt of a waiter (executor shutdown interrupts every idle worker),
and the one remaining miss of MON14-1 (a registry lookup that finds no object) disappears.
**Cost.** Medium: a registry field, and the two callers (interpreter-round files: a patch page).
**Risk.** Low (the `Arc` keeps a pruned monitor alive exactly as a waiter's own `Arc` does).
**First step.** Read `update_thread_objs_after_gc` and the prune path to confirm a waiter's
monitor is never pruned while waited on (it is not idle: the waiter is in its wait set).

## MW4-3. Retire the broadcast `wake_waiters_for_interrupt`

**What.** Once `r14w4-monitor2-interrupt-wakes-only-its-target-callers-patch-FIXED-20260929.md` is
applied, `MonitorTable::wake_waiters_for_interrupt` has no production caller; the targeted
`wake_waiter_for_interrupt` covers the switch-off arm itself. Move the existing interrupt tests
to the targeted call (each already names its waiter's `ThreadId`) and delete the broadcast.
**Benefit.** One interrupt entry point, so a future caller cannot pick the one that wakes
everybody. **Cost / risk.** Tiny / none. **First step.** `rg -n wake_waiters_for_interrupt` after the
patch lands.

## MW4-4. `wait(ms)` parks against a deadline, not a duration

**What.** The timed loop recomputes `remaining` from `Instant::now()` on every return and parks
`remaining.min(slice)` with `wait_for`; `parking_lot` also offers `wait_until(Instant)`. Parking
until the deadline itself (capped by the safety slice's own deadline) removes one `Instant::now()`
per return and any drift from a return that re-parks just before the deadline for a sub-ms
remainder. **Benefit.** Marginal (one clock read per wake; exactness at the deadline).
**Cost / risk.** Tiny / low. **First step.** Only if MW4-1's census or a probe shows timed waits
returning late.
