# `AutoScalingEventExecutorChooserFactoryTest.testScaleUp` scaled to 3 — FIXED 2026-08-29: a timed park's deadline was rounded up to the Windows system tick

## Status

**FIXED.** `fix/netty-autoscale-and-npe-residuals-20260829`, commit
`2d855f294`. `AutoScalingEventExecutorChooserFactoryTest`, one binary, the fix
A/B'd against itself with `CRATONVM_WIN_HIRES_PARK`:

| arm | pass | fail |
|---|---:|---:|
| `CRATONVM_WIN_HIRES_PARK=0` (pre-fix behaviour) | 60 | **29** |
| default (fixed) | **89** | **0** |

89 runs per arm over five batches, the last four interleaved run-for-run so
host drift lands on both arms equally: 12/12 vs 10/12, 25/25 vs 22/25, 20/20 vs
15/20, 20/20 vs 7/20, and 12/12 vs 6/12 after merging 121 commits of `dev`
and rebuilding. The OFF arm's rate is not stable
between batches (2/12 to 13/20) and is not expected to be — it is a beat
between two ~50 ms periods, so its phase, and with it the fraction of runs that
step over the window, drifts with anything that moves either one. What is
stable is the fixed arm: 77 runs, 0 failures.

Also `regression-suite/run.sh` 77/77 on the post-merge binary, and a 150-class
netty slice (every
`io.netty.util.*`, `io.netty.channel.*`, `io.netty.resolver.*`,
`io.netty.bootstrap.*`, `io.netty.handler.proxy.*`) run on the pre-branch and
post-branch binaries: identical, except `ProxyHandlerTest` going 8 failures to
0 (its own page).

Superseded page:
`known-issues/netty/autoscalingeventexecutorchooserfactorytest-scaled-too-far-20260829.md`.

## What the page asked for, and what each answer was

The page listed three things it had not done. All three are done, and the
first two are what pointed at the answer:

* **HotSpot A/B on the identical class** — HotSpot 8/8 clean, ~4.5 s per run.
  CratonVM 7/10, ~18 s per run.
* **A second CratonVM run, to see whether "3" is deterministic** — it is not:
  3 failures in 10 runs on the same binary and the same quiet host. So the
  page's own timing-sensitivity hypothesis was right in shape.
* **Read what the scale-up trigger keys on** — a per-cycle busy-streak count,
  and reading it is what showed the test is not asserting on a settled state
  at all.

The page's guess about the *mechanism*, though, was that CratonVM's raw
throughput on the stress workload differed. It does not: the stressed
executor reports the same ~0.70-0.75 utilization on both VMs, cycle after
cycle. What differs is the length of the monitor's cycle.

## The test asserts on a state that lasts one cycle

`AutoScalingEventExecutorChooserFactory`'s monitor counts, per cycle, how many
active executors have been above `scaleUpThreshold` for `scalingPatienceCycles`
consecutive cycles, and wakes that many (capped by `maxRampUpStep`). It does
**not** reset the busy streak after acting. So a single executor held at 70%
load keeps qualifying, and the group does not settle at 2 — it climbs to
`maxThreads`, sheds idle threads, and climbs again.

`ScaleTimelineProbe` (see `internal/repros/netty-park-cadence-20260829/`)
samples `activeExecutorCount()` every 1 ms instead of the test's 50 ms. On
**HotSpot**:

```
[ 1623,71] active 1 -> 2   e0{u=0,000 susp=false} e1{u=0,000 susp=true}  e2{u=0,719 susp=false}
[ 1673,90] active 2 -> 3   e0{u=0,011 susp=false} e1{u=0,000 susp=false} e2{u=0,687 susp=false}
[ 1774,67] active 3 -> 2   ...
[ 1823,35] active 2 -> 3   ...
[ 1874,12] active 3 -> 1   ...
```

The oscillation is identical on CratonVM. Neither VM ever holds 2. The test
passes on HotSpot because it *samples* 2 while passing through, and the
sampling is the whole game:

```java
while (group.activeExecutorCount() < 2 && System.nanoTime() < deadline) {
    Thread.sleep(50);
}
assertEquals(2, group.activeExecutorCount(), "Should scale up to 2 ...");
```

The window in which the count reads 2 is exactly one monitor cycle wide. The
test steps through that window in strides of `Thread.sleep(50)`. If the window
is 50 ms and the stride is 50 ms, essentially every pass lands inside it. If
the window is *shorter* than the stride, a pass can step over it — and the next
sample reads 3, which exits the loop and fails the assertion.

So the question is only: how long is one monitor cycle?

## The measurement

`GeePeriodProbe` times the real firing gaps of
`GlobalEventExecutor.scheduleAtFixedRate(50 ms)` — the monitor's own timer —
with `System.nanoTime()`.

| arm | p50 | mean | gaps < 49 ms |
|---|---:|---:|---:|
| CratonVM, pre-fix | 46.9 ms | 49.9 ms | 62/79 |
| HotSpot 25 | 49.1 ms | 50.1 ms | 39/79 |
| CratonVM, fixed | 50.0 ms | 49.8 ms | 1/79 |

**The mean was already right, and that is why nothing had noticed.** netty
re-arms a fixed-rate task from an ABSOLUTE deadline (`deadlineNanos += period`),
so a wait that overshoots does not accumulate — it steals the overshoot back
from the next cycle. An average of 50 ms says nothing at all here.

The pre-fix gap sequence says everything:

```
466, 453, 465, 470, 629, 468, 472, 467, 471, 619, 475, 460, 465, 470, 614, ...
```

Four cycles of ~46.9 ms, then one of ~62.5 ms. That is a 50 ms deadline
landing on a 15.625 ms grid: fire at 62.5, 109.4, 156.3, 203.1, 250.0 —
gaps of 46.875 x4 then 62.5, repeating with period 250 ms. Four cycles in five
are shorter than the test's 50 ms stride.

## Root cause

Windows rounds a timed wait up to the system clock tick, and the default tick
is 15.625 ms. Measured on this host, 50 ms requested:

| primitive | p50 |
|---|---:|
| `kernel32 Sleep(50)` | 62.3 ms |
| `WaitForSingleObject(event, 50)` | 62.2 ms |
| `SleepConditionVariableSRW` (std `Condvar`) | 62.2 ms |
| `parking_lot::Condvar::wait_for` | 62.4 ms |
| waitable timer, ordinary | 62.2 ms |
| **waitable timer, `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`** | **50.3 ms** |
| `std::thread::sleep` | 50.4 ms |

`timeBeginPeriod(1)` moves none of them — it returns success, the
system-wide resolution was already 1.0 ms by `NtQueryTimerResolution`, and
every row above reads the same before and after. The high-resolution waitable
timer is the only primitive that is accurate, and it is exactly what Rust's
`std::thread::sleep` uses internally.

That last row is why the defect was invisible from the obvious direction:
CratonVM's `Thread.sleep(50)` was already *more* accurate than HotSpot's
(+0.43 ms vs +0.55 ms mean overshoot), because `native_thread_sleep` bottoms
out in `std::thread::sleep`. Every OTHER timed wait in the VM —
`LockSupport.parkNanos`, and thus `Condition.awaitNanos`,
`BlockingQueue.poll(timeout)`, `Future.get(timeout)` — bottoms out in
`ParkState::park_interruptible`'s `parking_lot` condvar, and inherited the
tick.

netty's `GlobalEventExecutor.takeTask` is
`taskQueue.poll(delayNanos, NANOSECONDS)`, i.e. that path.

## The fix

`vm/src/threading/jvm_thread.rs`. A **timed** park now waits on two handles at
once: this thread's high-resolution waitable timer, and a per-`ParkState`
auto-reset event that `unpark` signals alongside the condvar notify. The
untimed park is untouched.

Three things make that safe rather than merely accurate:

* **The permit protocol does not change.** The lock is released before
  blocking, and `unpark` takes that same lock to set the permit *before* it
  signals the event, so a signal landing in the gap leaves the auto-reset event
  set and the next wait returns immediately. A stale signal costs one spurious
  return, which `LockSupport.park` is specified to allow.
* **Interrupts stay prompt.** `thread_interrupt` already calls
  `park_state.unpark()`, so an interrupt now reaches the accurate wait through
  the event, not only through the slice poll.
* **The wakeup rate does not go up.** `park_interruptible` keeps slicing, but
  the accurate slice is **15 ms, not 5 ms**. A 5 ms condvar slice never cost a
  wakeup every 5 ms — the OS rounded it out to ~15.6 ms — so asking the timer
  for 5 ms would have tripled the wakeup rate of every timed park in the VM to
  buy interrupt latency nothing was waiting on.

`CRATONVM_WIN_HIRES_PARK=0` restores the condvar wait. Every table on this page
is one binary against itself through that lever.

That last claim is measured, not asserted. `ParkCostProbe`, 16 threads each
`LockSupport.parkNanos(50 ms)` in a loop for a fixed window, with the caller
timing process CPU:

| arm | parks | mean park | process CPU | CPU per park |
|---|---:|---:|---:|---:|
| `CRATONVM_WIN_HIRES_PARK=0` | 2144 | 59.9 ms | 1.55 s | 0.72 ms |
| default (fixed) | 2544 | 50.4 ms | 1.59 s | **0.63 ms** |
| HotSpot 25 | 1904 (6 s window) | 50.5 ms | — | — |

19% more parks completed for 3% more CPU, i.e. slightly *less* CPU per park —
the slice count per park is unchanged, and each park is shorter. The
`meanPark` column is also the sharpest statement of what was wrong: 59.9 ms
for a 50 ms request, against HotSpot's 50.5 ms.

A unit test carries the ceiling:
`park_state_timed_park_does_not_overshoot_to_the_system_tick`. The existing
`park_state_park_with_timeout` asserts a 40 ms *floor* and could never have
seen this; the new one asserts the other side, and fails at 61.2 ms with the
flag off.

## Why this is a Windows-only fix, and what it does not cover

Linux `parking_lot` waits are `FUTEX_WAIT` with a `timespec` and have no such
grid, so the module is `#[cfg(target_os = "windows")]` and every other target
keeps the condvar path unchanged.

`Object.wait(timeout)` still takes 5 ms condvar slices in
`threading/monitor.rs` and so still overshoots its deadline by up to a tick.
It is deliberately not touched here: its loop reads `wait_for`'s
`timed_out()` to tell a delivered `notify` from a timeout (the
`pending_notifies` protocol, `CRATONVM_MONITOR_PENDING_NOTIFY`), so swapping
the primitive is a protocol change rather than a timeout change — and no
evidence tied it to this failure. It overshoots; it never fires early, which
is the direction that broke this test.

## Scope

Not a netty defect. Any Java code on Windows whose cadence comes from a timed
park — every `ScheduledExecutorService`, every netty `EventLoop` scheduled
task, every `poll(timeout)` loop — was running on a 15.625 ms grid, with the
right mean and a four-short-one-long period. This test is simply the one that
asserted on a state narrower than the error.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.concurrent.AutoScalingEventExecutorChooserFactoryTest
# and the cadence itself, which is the thing to measure:
cratonvm.exe --java-home <jdk25> -cp <netty-common> \
  io.netty.util.concurrent.GeePeriodProbe
```

## Related

* `internal/repros/netty-park-cadence-20260829/` — the four probes and what
  each one ruled out.
* `known-issues/netty/parameterizedsslhandlertest-residual-stalls-20260824.md`
  — another netty page whose symptom is a cadence, not a result. Not
  re-measured against this fix.
