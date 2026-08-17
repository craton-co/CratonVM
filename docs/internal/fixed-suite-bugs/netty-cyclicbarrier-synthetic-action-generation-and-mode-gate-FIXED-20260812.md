# `CyclicBarrier(parties, Runnable)` — the barrier action was silently dropped

**Status:** ✅ FIXED (2026-08-12) in `native-builtins/src/lib.rs`,
`native-builtins/src/util_concurrent_ext.rs` and `vm/src/vm/vm_init.rs`. Found
while fixing the timed-wait family for
[netty investigate-batch-01](investigate-batch-01.md); not itself a batch-01
failure, but it lived in the same native.

The synthetic `CyclicBarrier` turned out to carry **three** defects, not one.
Fixing the headline one alone would have left a barrier that deadlocks on reuse
and a JDK mode with no `CyclicBarrier` constructor at all.

## The three

### 1. The barrier action was discarded (the original report)

`67c5e048c` had already narrowed the blast radius: the synthetic
`CyclicBarrier` surface is gated behind synthetic-AQS mode, so the **default
real-JDK build runs the real bytecode** and was never affected. Under
`CRATONVM_SYNTHETIC_AQS=1` the `Runnable` passed to
`new CyclicBarrier(parties, barrierAction)` never ran — HotSpot runs it once per
trip, on the last arriving thread, before any party is released.

```rust
fn native_cb_init_action(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Simplified: ignore the barrier action Runnable
    native_cb_init(ctx, args)
}
```

`getParties()` and `isBroken()` were correct; only the action was lost.

**Why it had not been fixed in place:** the native's state lived in an `int[3]`
"holder" array written into **object slot 0** of the receiver. In real-JDK mode
slot 0 of `java.util.concurrent.CyclicBarrier` is the `lock` field (a
`ReentrantLock` reference), so the holder already type-puns one reference slot;
parking a `Runnable` in another slot compounds that, and a reference stored into
a slot the class declares as an `int` is exactly the shape the GC's W7-84 guard
reports and auto-boxes.

### 2. The barrier had no generation, so reuse deadlocked

Recorded as a comment in `util_concurrent_ext.rs` and never fixed. A trip reset
one shared `count` to 0 and notified; every waiter decided it had been released
by re-reading `count == 0`. That test is only valid while nobody re-enters the
barrier. Under a tight loop a released waiter can be preempted before it
re-reads; by then a faster party has bumped `count` back to 1.., so the waiter
concludes it was NOT released and waits again — with its wake-up already spent.
The barrier is then permanently one party short and every subsequent trip
deadlocks.

The comment sized it at 4 parties over 20 rounds. It is cheaper than that:
**2 parties over 10 rounds hangs on the first reuse**, which is why the probe
below never reaches its third stage on an unfixed build.

### 3. Synthetic-JDK mode had lost the constructor entirely

The gate `67c5e048c` installed is on the **flag** (`CRATONVM_SYNTHETIC_AQS`),
and a flag is not a mode. Synthetic-JDK mode does not set that flag, and it has
no real `CyclicBarrier` bytecode either — `classloading`'s
`synthetic_stub_fields` gives the class a 3-field compatibility stub with no
method bodies. So the gate removed the natives from the one mode that cannot
live without them:

```
$ cargo test -p cratonvm-vm --features synthetic-jdk --test interpreter_tests s49_cyclic_barrier
test result: FAILED. 0 passed; 4 failed
  testCyclicBarrierGetParties      — java.lang.NoSuchMethodError: java.util.concurrent.CyclicBarrier.<init>(I)V
  testCyclicBarrierIsBroken        — (same)
  testCyclicBarrierGetNumberWaiting— (same)
  testCyclicBarrierReset           — (same)
```

Measured on pristine dev `8763197f2`. The corpus's own known-failure list is
documented as *empty and finished*, and `runtime/tck.rs` still registered all
four as `TckTest::passing`, so the gate was red and reading green.

## What the fix does

**Storage.** Receiver slot 0 now holds a REFERENCE array of length 2:

```
[0] = long[4] { parties, count, generation, broken_gen }
[1] = the barrier action Runnable, or null
```

The extra level is what lets the action be stored without punning a second
slot: one reference array holding one primitive array and one `Runnable` is
type-clean under both layouts, and costs one indirection per access. Slot 0 was
already carrying an array, so nothing new is punned.

**Generation.** A waiter records the generation it arrived in and is released
iff the generation has moved past it. The counter only ever increases, so the
next round starting cannot undo a waiter's release test — defect 2 gone. It is
`long`, not `int`, so the "is this generation broken" comparison cannot alias by
wraparound.

**`broken_gen`, not a `broken` flag.** `reset()` has to break the generation its
parked parties are waiting in — they must wake with `BrokenBarrierException` —
while leaving the FRESH generation unbroken so `isBroken()` reads false
immediately afterwards. A single flag cannot express that: the waiters have not
run yet when `reset()` returns, so by the time they look, the flag they needed
to see is already cleared. `isBroken()` is now `broken_gen == generation`.

**The action runs where HotSpot runs it** — on the last party in, still holding
the monitor, before any party is released, which is where
`CyclicBarrier.nextGeneration` runs it under its own `ReentrantLock`. If it
throws, the barrier is broken and the failure propagates, so no party proceeds
past a trip whose action did not complete. The `Runnable` is rooted through a
`NativeHandleScope` across the call, as is the receiver and both holder arrays.

**Registration.** The natives are now registered from two places, because two
different conditions need them and neither can see the other:
`register_concurrent_natives` when `CRATONVM_SYNTHETIC_AQS` is set (unchanged),
and `vm_init`'s synthetic-JDK arm, which is where the JDK *mode* is known —
exactly the precedent `register_synthetic_aqs_natives` set one class earlier for
`Semaphore`/`ReentrantLock`.

## The question the old record left open

> "The remaining question is whether the synthetic-AQS surface needs a barrier
> action at all, or whether that mode should also be retired. Prefer **deleting
> these natives** over extending them."

Deleting them is not available, and defect 3 is why: synthetic-JDK mode has no
`CyclicBarrier` bytecode to fall back to. "Run the real class instead" is the
right answer for the default build — `67c5e048c` already gave it — and is not an
answer at all for the mode that has no real class. So the natives had to be
made correct rather than removed.

## Result

`CbProbe.java` — 13 rungs: action count, which thread ran it, whether any party
was released first, the arrival index, 10 rounds of reuse, a 4-party/20-round
generation stress, `getNumberWaiting`, `reset()` waking a parked party,
`isBroken()` after reset, timed `await` and its elapsed time, `isBroken()` after
timeout, and `await` on a broken barrier.

| | HotSpot JDK 25 | CratonVM default | CratonVM `SYNTHETIC_AQS=1` before | after |
|---|---|---|---|---|
| `barrierActionRuns` | 1 | 1 | **0** | 1 |
| action ran on the last arrival | true | true | **false** | true |
| parties released before the action | 0 | 0 | 0 | 0 |
| `tripsOver10Rounds` | 10 | 10 | **hangs** | 10 |
| `generationWorkersCompleted` (4 parties, 20 rounds) | 4/4 | 4/4 | **not reached** | 4/4 |
| `numberWaitingWithOneParked` | 1 | 1 | not reached | 1 |
| `reset()` wakes a waiter with | `BrokenBarrierException` | same | not reached | same |
| `isBroken()` after `reset()` | false | false | not reached | false |
| timed `await` | `TimeoutException` @500 ms | same | not reached | same |
| `isBroken()` after timeout | true | true | not reached | true |
| `await` on a broken barrier | `BrokenBarrierException` | same | not reached | same |

All 13 rows now match HotSpot in both modes.

The synthetic-JDK corpus (the blocking gate) goes from 4 failures to none:

```
cargo test -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
  pristine 8763197f2 : 4 failed  (the s49_cyclic_barrier family)
  with this change   : test result: ok. 924 passed; 0 failed
```

And the netty classes the un-fixed generation was blamed for, run under
`CRATONVM_SYNTHETIC_AQS=1`:

```
io.netty.util.NettyRuntimeTests            before: found=7 ok=5 failed=2  ms=240940
                                            after: found=7 ok=7 failed=0  ms=649
io.netty.util.concurrent.DefaultPromiseTest before: found=20 ok=20        ms=42897
                                            after: found=20 ok=20         ms=42407
```

`NettyRuntimeTests` was not failing on its merits — it was spending 241 seconds
deadlocked against per-test timeouts. `DefaultPromiseTest` was never affected,
which is worth recording: the comment named three classes and only one of them
was actually this bug.

## Regression cover

`native-builtins/src/lib.rs`, module `cyclic_barrier_tests` — seven tests, all
reachable single-threaded because `await()` with `parties == 1` IS the last
arrival: the action runs once per trip; a one-arg barrier runs nothing; the
holder keeps a `Long` state array and a `Reference` action slot with the action
in it (the anti-punning contract); a trip advances the generation and zeroes the
count; `reset()` breaks the old generation and still reports unbroken; a
throwing action breaks the barrier and keeps it broken; and both constructors
reject `parties <= 0`.

## Related

Two other gaps in the same native were fixed earlier the same day in
`docs/internal/fixed-suite-bugs/netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md`:
the `TimeUnit` ordinal was read from the wrong slot (so `await(30, SECONDS)`
waited 30 ms), and barrier failures were reported as `IllegalStateException`
instead of the declared checked exceptions.

## Repro

```bash
# CbProbe.java — the 13 rungs above; compare against `java`.
javac -d /tmp/cb CbProbe.java
CRATONVM_SYNTHETIC_AQS=1 <cv-bin> --java-home "$JAVA_HOME" -cp /tmp/cb CbProbe
```

The probe carries its own watchdog: on an unfixed build it prints
`PROBE_TIMEOUT stage=reuse-action` and halts, which names the deadlock instead
of hanging the harness.
