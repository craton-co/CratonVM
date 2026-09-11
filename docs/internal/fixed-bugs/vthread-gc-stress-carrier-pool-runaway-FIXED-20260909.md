# `VthreadGcStress` wedges: the starvation watchdog read every stop-the-world pause as starvation, and the carriers it added made the next pause slower

**Status: FIXED 2026-09-09** (`vm/src/threading/virtual_threads.rs`, one sample
gate; `vm/src/threading/gc_barrier.rs`, the pause epoch it reads).
8 hangs in ~20 on the merge tip; 0 in 14 after, with the carrier pool pinned at
its base 32 instead of running away to 233+.

## The symptom

`vm/tests/vthread_probe_regression.rs::vthread_gc_stress_completes` timed out at
its 120 s cap. The VM alone, no harness:

```bash
./target/release/cratonvm -c vm/tests/resources/vthread_probe/classes VthreadGcStress
```

| arm | runs | result |
|---|---|---|
| default | 6 | 201 s, 26 s, 30 s, 201 s, 44 s, 21 s — **2 wedged past even a 200 s cap** |
| `CRATONVM_DISABLE_JIT=1` | 3 | 9 s, 7 s, 8 s — tight, never wedges |
| `CRATONVM_XT_JIT_ROOT_SCAN=0` | 6 | 5–6 s — never wedges |

Bimodal, like its predecessor, and the probe COMPLETES its work when it
completes at all (`counted=3000 ok=true`). So this is a livelock, not a
deadlock, and not the lost-arrival hang
`vthread-probe-intermittent-hang-FIXED-20260905.md` fixed: the stop-the-world
census shows the last pause of a wedged run reaching `arrived=N expected=N`.
Nothing is waiting at the barrier.

## Three readings that were wrong, and what killed each

Worth recording, because each is the obvious first guess:

* **"The sleep timer never re-queues the virtual thread."** Refuted by running
  the same workload with the vthread body doing pure computation and no
  `Thread.sleep` at all: still wedges.
* **"`done.countDown()`'s unpark of the main thread is lost."** Refuted by
  replacing `done.await()` with a spin on `getCount()`: a main thread that never
  parks freezes too, which means the freeze is global rather than a wakeup.
* **"The carriers are blocked on something."** Half true and useless on its own.
  `dispatch_count` frozen says nothing about WHY.

## What it actually was

`CRATONVM_DBG_CARRIER=1` (added with this fix) prints one line per
starvation-watchdog sample. The wedge:

```text
[carrier] queued=275 busy=233 live=233 dispatch=5567 moved=false   (forever)
```

and the same workload with the cross-thread JIT root scan off:

```text
[carrier] queued=0 busy=0 live=32 dispatch=6000 moved=true         (completes)
```

`live=233` against a base pool of 32 is the whole finding. The pool GREW there,
and it grew for a reason that is not starvation:

1. A stop-the-world pause parks every carrier, so `dispatch_count` **cannot**
   move across one.
2. That is exactly `carrier_pool_is_stalled`'s signature —
   `queued > 0 && busy >= live && !dispatch_moved`. Every pause therefore reads
   as starvation, and after `CARRIER_STALL_SAMPLES` consecutive such samples the
   watchdog adds a carrier.
3. Each added carrier is one more OS thread for the next pause to stop — and on
   Windows one more for `xt_root_scan::take_over_pass` to `SuspendThread` +
   `GetThreadContext` + `ResumeThread`, which that pass does for **every thread
   in the process on every collection**.
4. Slower pause → more stalled samples → more carriers → slower pause.

It terminates only at `MAX_CARRIER_THREADS` (256), by which point a collection
is suspending and resuming ~250 threads and the VM makes no observable
progress. This probe drives 400 `System.gc()` rounds against 3000 virtual
threads, which is why it finds it and ordinary workloads do not.

The instrumented run counts **320–711 pause-contaminated samples per run** —
every one of them previously pushing the watchdog toward growth.

## The fix

The watchdog now drops any sample whose interval contained a pause:

```rust
let (pause_epoch, pause_now) = crate::threading::gc_barrier::stw_pause_state();
let paused_in_interval = pause_now || pause_epoch != last_pause_epoch;
last_pause_epoch = pause_epoch;
if paused_in_interval { continue; }
```

`continue`, deliberately, rather than resetting `stalled_samples`. Resetting
would disarm the watchdog for the case it exists for — a carrier blocked in
`monitorenter` under a GC-heavy workload would never accumulate the consecutive
samples growth is gated on. Skipping neither counts the pause as a stall nor
forgets genuine ones: they resume accruing from the first pause-free interval.

`STW_PAUSE_EPOCH` / `STW_PAUSE_IN_PROGRESS` are process-global rather than
`GcBarrier` fields because the reader is started by
`VirtualThreadManager::start_carriers` and has no barrier handle — and must not
be given one, since the unit tests construct a bare manager with no VM around
it. Both halves are read: `in_progress` catches a pause the sampler is sitting
inside, and the EPOCH catches one that began and ended entirely within the
sampling interval, which is the common case at 400 collections.

## Results

| | before | after |
|---|---|---|
| `VthreadGcStress`, 6+8 runs | 2/6 wedged past 200 s | **0/14** |
| carrier pool, peak | 233 (cap 256) | **32** (= base) |
| wall clock when it completed | 21–44 s | 13–37 s |
| `vthread_probe_regression`, 3 runs | timeout | 36.8 s / 13.9 s / 12.1 s |

## What this does NOT fix

**`xt_root_scan::take_over_pass` is still O(threads in the process) per
collection**, with a `SuspendThread`/`GetThreadContext`/`ResumeThread` round
trip each. That is why the same probe runs in 5 s with
`CRATONVM_XT_JIT_ROOT_SCAN=0` and 13–37 s with it on — a 3–7x cost that this
change does not touch and that no reading currently attributes. Removing the
feedback loop removes the wedge, not the per-collection cost; a pool that is
legitimately large for a legitimately concurrent workload still pays it on every
collection. That is the next thing to measure here.
