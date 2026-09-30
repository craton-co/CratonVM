# JIT round 14 lane sync: proposals (synchronized calls, contended monitors)

Status: OPEN (proposal book; ideas, not work items)
Area: `jit/src/lib.rs` (IR admission of synchronized methods), `vm/src/runtime/interpreter/jit_bridge.rs` (synchronized doors), `jit/src/runtime_lowering.rs` (inline spin), `vm/src/threading/monitor.rs`
Severity: proposals
Found by: round 14 wave 1 lane sync

Ranked by expected benefit per unit of risk. Nothing here was measured by the lane (no builds,
no runs); each names what decides it.

## SY14-1. Let the synchronized door enter a precise-frame optimizing body that has guard exits

**What.** With `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED=1` (round 14 wave 1) a synchronized
method with an exception table may take the optimizing tier, but `lib.rs` discards any such body
the compiled caller's synchronized door would refuse: `jit_bridge.rs`
`optimizing_exits_route_exactly` accepts an IR body of a method with a table only when
`ir_osr_exception_exit_bcis` is `Some` and uncovered, and that fact is `None` for every body with
a guard exit. The refusal exists because an IR call-exception exit publishes no frame; on the
precise-frame route every exit inside a protected range DOES publish one (the rethrow pads, the
RBC.6 promise), so the refusal is too strict there. Publish the route on the artifact
(`CompiledMethod::ir_precise_exception_frames: bool`, set by `ir_lower` when it was handed
protected ranges) and have `optimizing_exits_route_exactly` answer `true` for it; then drop the
round-14 discard in `lib.rs` (search `the synchronized door could not enter`).
**Benefit.** The guard-bearing half of the W11-2 population (most real bodies: any speculative
guard) gets its optimizing body through the compiled-caller door too. **Cost.** Small: one field,
one disjunct, one test. **Risk.** Low-medium: the door's guard exits are resumed by the ordinary
stash sinks, which already serve a synchronized wrapped body. **First step.** The count of
`[ir] ... the synchronized door could not enter this precise-frame body` lines
(`CRATONVM_DBG_JITC=1`) over the R14 battery and one Spring run with the switch on; if near zero,
skip.

## SY14-2. A registered inline spinner instead of a helper call when an entrant is parked

**What.** Compiled code's inline spin leaves for the helper the moment `entry_waiters != 0`
(`runtime_lowering.rs` `emit_inline_inflated_spin_acquire`), because an uncounted spinner would
let a release wake a parker it will then beat. The helper's spin counts itself in `spinners`
(`SpinnerGuard`) and so suppresses that wake. The inline spin could do the same: `LOCK ADD DWORD
[RCX + spinners], 1` at the first poll that sees a parker, `LOCK SUB` on a win, and on a give-up
go to the helper WITH the registration still held so the helper's `SpinnerGuard` drop performs
the "spinner that gives up with the monitor free wakes on its way out" duty. **Benefit.** One
helper round trip (and its GC-safe spin/park preamble) fewer per contended enter while any thread
is parked -- the oversubscribed case of `ThreadChurn 192`. **Cost.** Medium: a new helper entry
that adopts a registration, the arm's register budget (RAX is free at the poll). **Risk.**
Medium: the lost-wake-up argument of `Monitor::wake_successor` must be re-made for an adopted
registration. **First step.** The round-14 census (`r12w8-monitor-where-a-contended-enter-spends-its-time-20260927.md`,
"Round 14 wave 1"): if `inline_spin_waiter_exits` is a small share of the inline spin's
outcomes, drop this.

## SY14-3. Drain the inline-spin census at the exit report

**What.** `Monitor::drain_inline_spin_census` runs only in the helper's spin, so the wins of a
monitor no thread later takes to the helper are never reported. Walk the VM's `MonitorTable`
index once at the census exit report (the report would need the table: give
`report_monitor_notify_census_at_exit` an optional `&MonitorTable`, or drain from the VM's own
shutdown path) and drain every monitor. **Benefit.** Exact `inline_spin_wins`. **Cost / risk.**
Small / none (diagnostic only). **First step.** Only if the M2-1 census's win counts look short
against `[monitor-inline census]`'s inline enters.

## SY14-4. `Thread.holdsLock(C.class)` folded in a static self-locking body

**What.** Round 13 wave 13 folds `Thread.holdsLock(this)` to `true` in an instance self-locking
body (`x64/frames.rs` `self_lock_holds_lock_fold_at`). The static twin is `ldc C; invokestatic
holdsLock` inside a `static synchronized` method of `C` itself; the fold would match the `ldc`
by its resolved class id against the method's declaring class. **Benefit.** Small (assertion-
style code in static synchronized methods; `R13Sync5HandlerTable`'s static half). **Cost.**
Small. **Risk.** Low (the static body holds its mirror from prologue to every exit exactly as the
instance one holds `this`). **First step.** Grep a Spring census for `holdsLock` sites inside
static synchronized methods; skip if none.
