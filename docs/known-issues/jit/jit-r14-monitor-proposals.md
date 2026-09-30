# JIT round 14 lane monitor: proposals (wait/notify after M2-3 / M2-4)

Status: OPEN (proposal book; ideas, not work items)
Area: `vm/src/threading/monitor.rs` (`Monitor::wait`, `MonitorState`, `wake_all_for_interrupt`), `vm/src/vm/vm_exec.rs` (`thread_interrupt`, `monitor_wait_with_state`)
Severity: proposals
Found by: round 14 wave 3 lane monitor

Ranked by expected benefit per unit of risk. Nothing here was built or measured by the lane.
Each builds on round 14 wave 3's per-waiter condvar (`WaitEntry::signal`,
`CRATONVM_MONITOR_NOTIFY_ONE_WAITER`) and backed-off poll (`CRATONVM_MONITOR_WAIT_POLL_BACKOFF`).

## MON14-1. The interrupt wake wakes only the interrupted waiter

**What.** `Thread.interrupt()` of a thread in `Object.wait()` calls
`MonitorTable::wake_waiters_for_interrupt(obj)`, which still wakes EVERY waiter on the monitor
(`Monitor::wake_all_for_interrupt` walks the wait set): each takes the state mutex, finds no
credit and no flag, and re-parks. With a condvar per waiter the target can be singled out: carry
the waiter's `ThreadId` in `WaitEntry` and give `wake_waiters_for_interrupt` the target's id
(`vm_exec.rs` `thread_interrupt` and `debug/inspect.rs` `wake_quiesced` already know it), so only
the matching entry is signalled; the shared-condvar arm keeps `notify_all`.
**Benefit.** Executors that interrupt idle workers on shutdown / `cancel(true)`: one wake instead
of N per interrupt. **Cost.** Small (one field, one parameter, two callers outside this file:
patch page). **Risk.** Low: a missed target costs one poll slice, as today when the lookup misses.
**First step.** A unit test: 8 waiters, interrupt one, count signalled returns (1, not 8).

## Round 14 wave 4 (lane monitor2): MON14-1 landed

Monitor half landed in `vm/src/threading/monitor.rs` behind `CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET`
(default on, per VM through `MonitorTuning`): `WaitEntry::waiter`, `MonitorState::signal_waiter_for_interrupt`,
`Monitor::wake_for_interrupt_of`, `MonitorTable::wake_waiter_for_interrupt(obj, target)` (the
switch off wakes every waiter as before). The two callers (`vm_exec.rs` `thread_interrupt`,
`debug/inspect.rs` `wake_quiesced`) are interpreter-round files: they switch over with
`r14w4-monitor2-interrupt-wakes-only-its-target-callers-patch-FIXED-20260929.md`; until then the
broadcast `wake_waiters_for_interrupt` keeps serving them unchanged. Tests
`mon14_1_the_interrupt_wake_signals_only_its_target_entry` (9 entries: the target's alone is
signalled), `mon14_1_a_targeted_interrupt_wake_returns_only_the_target`,
`mon14_1_the_targeted_wake_never_inflates_an_untouched_object`.

## MON14-2. Check the interrupt flag under the state lock before the first park

**What.** `monitor_wait_with_state` tests the flag, then publishes `jmx_waiting_monitor`, then
enrols. An interrupt landing between the test and the enrolment is not woken (the waker either
finds no monitor or signals before the enrolment) and is seen only at the first poll slice --
which is why M2-4 keeps the first slices at 5 ms. Test the flag once more right after
`enroll_waiter`, under the state lock the waker also takes; the window closes, and the backoff
could start at 20 ms. **Benefit.** Interrupt latency exact (no 5 ms first-slice dependence).
**Cost / risk.** Tiny / low (the outcome, `Interrupted`, is what the first poll would report).
**First step.** A deterministic unit test that sets the flag between `enter` and `wait` without
a wake and asserts the wait returns without a full slice.

## Round 14 wave 4 (lane monitor2): MON14-2 landed

`Monitor::wait` reads the interrupt flag once more right after `enroll_waiter`, under the state
lock, and ends the wait `Interrupted` without parking (`CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK`,
default on; a `_ if interrupted_at_enrol` arm ahead of the two park loops). With it (and the
back-off on) the poll starts at 20 ms (`WAIT_POLL_SLICE_RECHECKED`) instead of 5 ms, and 5 ms
again only when the watchdog's stack dump is already requested at enrolment. The argument: the
interrupter stores the flag (Release) before it takes the registry's waiting-monitor mutex and
then the monitor's state lock; the waiter writes that registry slot before it takes the state lock
to enrol. Either the wake's state-lock section follows the enrolment (the waiter is in the wait
set, parked or about to re-test) or precedes it (its release carries the flag to this read); and
a lookup that found the slot empty came before the waiter's write of it, which carries the flag
the same way. Test `mon14_2_an_interrupt_before_enrolment_ends_the_wait_without_a_park` (timed
and untimed: an observer polling the wait set never sees the waiter, the flag is left set, the
monitor is re-held at depth 2).

## MON14-3. Pool the per-waiter condvars

**What.** Each `Object.wait()` allocates an `Arc<Condvar>` (M2-3). Keep a small free list in
`MonitorState` (`spare_signals: Vec<Arc<Condvar>>`, capped at e.g. 8): a waiter pops one when it
enrols and pushes its own back after it leaves the wait set, all under the state lock. A
`parking_lot` condvar holds no permit, so a signal that found nobody leaves nothing behind for the
next user. **Benefit.** One allocation and free fewer per wait (ping-pong hand-overs:
`R13Monitor2WaitNotify` `ping-pong`). **Cost / risk.** Small / low. **First step.** Measure first:
`R14MonitorNotifyOne` `t-tokens-*` and `R13Monitor2WaitNotify` against
`CRATONVM_MONITOR_NOTIFY_ONE_WAITER=0`; do this only if the allocation shows.

## MON14-4. A timed wait parks once for its whole timeout

**What.** With MON14-1/2 and the stack-dump wake patch
(`r14w3-monitor-stack-dump-wakes-backed-off-waiters-patch-FIXED-20260929.md`), every event a waiter
polls for has a wake of its own, so the poll can go: a timed wait parks for `remaining` in one
`wait_for`, an untimed one keeps only a long safety slice (1 s). **Benefit.** Zero idle wake-ups
(HotSpot's behaviour); `wait(ms)` wakes once at its deadline. **Risk.** Medium: any future
wake-less path would stall up to the safety slice. **First step.** The three prerequisites.

## Round 14 wave 4 (lane monitor2): MON14-4 landed

`CRATONVM_MONITOR_WAIT_SINGLE_PARK` (default on, per VM): a waiter parks for
`min(remaining, WAIT_SAFETY_SLICE = 1 s)` per `wait_for` (`MonitorTuning::wait_parks_once`,
`first_wait_slice`, `next_wait_slice`). Only when the notification credit, MON14-2's re-check and
M2-4's back-off are all on, so `CRATONVM_MONITOR_WAIT_POLL_BACKOFF=0` still restores the fixed
5 ms cadence and `CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK=0` the wave-3 back-off; the
`CRATONVM_WAIT_SPURIOUS_MS` diagnostic keeps its fixed slice. Wake-less paths re-checked by
reading: every cross-thread interrupt goes through `thread_interrupt` or `wake_quiesced` (both
wake; other `set_interrupted(true)` sites are self-interrupts), a notification signals the
marked entry's condvar, the stack dump wakes every indexed monitor
(`wake_all_waiters_for_stack_dump`) and a dump already requested at enrolment starts the wait at
5 ms. What still costs up to the safety slice: an interrupt whose registry lookup misses (no
`jmx_waiting_monitor` for the id the interrupter resolved) -- up to 100 ms before this. Tests
`mon14_4_a_waiter_parks_once_only_with_every_wake_in_place`,
`mon14_4_a_single_park_wait_keeps_its_deadline_and_its_notification`; the existing
`m2_4_..._still_sees_a_silent_interrupt` now sees the silent flag within a safety slice.

## MON14-5. M2-7 on the per-waiter condvar

**What.** `jit-r13-monitor2-proposals-RETIRED-20260929.md` M2-7 (defer a notified waiter's wake to the notifier's
release, HotSpot's entry-list move) needed a per-waiter identity, which M2-3 now provides:
`notify` could put the marked entry's condvar on a "wake at release" list that
`exit_reporting_release` drains, instead of signalling at once. **Benefit.** One wake-up per
hand-over when the notifier keeps the monitor after `notify`. **Risk.** Medium-high (the release
path is the compiled inline exit's too: the drain must happen on its helper edge).
**First step.** Census: how often `wait_reacquire_spin_wins` fails (the waiter parked twice).

## Round 14 wave 5 (lane monitor2): MON14-4 safety slice 250 ms, RV5-1 applied

`WAIT_SAFETY_SLICE` is 250 ms (was 1 s): an interrupt path nobody has found yet would stall a
waiter that long, not a second. The interrupt handshake is SeqCst on both sides (RV5-1 of
`jit-r14-review5-proposals.md`; see `r14w5-review5-wave4-review-findings-FIXED-20260929.md`, "Round 14
wave 5 (lane monitor2)").
