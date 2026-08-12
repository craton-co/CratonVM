# `CyclicBarrier(parties, Runnable)` — the barrier action is silently dropped

**Status:** OPEN, but **narrowed to synthetic-AQS mode** as of `67c5e048c`
(2026-08-12). Found while fixing the timed-wait family for
[netty investigate-batch-01](investigate-batch-01.md); not itself a batch-01
failure, but it lives in the same native.

**Default real-JDK mode is no longer affected.** `67c5e048c` (the batch-12/13
session) gates the whole synthetic `CyclicBarrier` native surface behind
synthetic-AQS mode, so the default build runs `CyclicBarrier`'s real bytecode
and the barrier action works. What remains is the synthetic path:

```
                                default mode   CRATONVM_SYNTHETIC_AQS=1
barrierActionRuns (expect 1)         1                  0
```

## Symptom

Under `CRATONVM_SYNTHETIC_AQS=1`, the `Runnable` passed to
`new CyclicBarrier(parties, barrierAction)` never runs. HotSpot runs it once
per trip, on the last arriving thread, before any party is released.

```java
AtomicInteger tripped = new AtomicInteger();
CyclicBarrier b = new CyclicBarrier(2, tripped::incrementAndGet);
Thread t = new Thread(() -> { try { b.await(); } catch (Exception e) { } });
t.start();
b.await();
t.join();
System.out.println("barrierActionRuns=" + tripped.get());
```

```
HotSpot JDK 25 : barrierActionRuns=1
CratonVM       : barrierActionRuns=0
```

`getParties()` and `isBroken()` are correct; only the action is lost.

## Root cause

`native-builtins/src/lib.rs`. `CyclicBarrier` is served by natives that
shadow the real bytecode, and the two-arg constructor is registered as

```rust
fn native_cb_init_action(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Simplified: ignore the barrier action Runnable
    native_cb_init(ctx, args)
}
```

The `Runnable` argument is discarded, so `cb_await_inner` has nothing to
invoke when the last party arrives.

## Why it was not fixed in place

The native's state lives in an `int[3]` "holder" array that is written into
**object slot 0** of the receiver. In real-JDK mode slot 0 of
`java.util.concurrent.CyclicBarrier` is the `lock` field (a `ReentrantLock`
reference), so the holder already type-puns one reference slot; parking a
`Runnable` in another slot compounds that, and a reference stored into a slot
the class declares as an `int` is exactly the shape the GC's W7-84 guard
reports and auto-boxes. Storing the action safely needs a rethink of the
native's storage model, not a one-line addition.

## Suggested fix

`67c5e048c` already took the first half of this advice — the natives no longer
shadow the real bytecode in default mode. The remaining question is whether the
synthetic-AQS surface needs a barrier action at all, or whether that mode
should also be retired.

Prefer **deleting these natives** over extending them.
`java.util.concurrent.CyclicBarrier` is pure Java over `ReentrantLock` +
`Condition`, `vm/src/vm/vm_init.rs` already asserts it loads as real bytecode,
and the timed-wait probe run for the batch-01 fix shows CratonVM's
`Condition.awaitNanos` / `LockSupport.parkNanos` are accurate to the
millisecond:

```
condition_awaitNanos30s_signalAt1500ms=SIGNALLED after 1501ms
condition_awaitNanos500ms_noSignal=elapsed=500ms
parkNanos500ms=elapsed=500ms
```

Running the real bytecode restores the barrier action, the real
`BrokenBarrierException`/`TimeoutException` types, and proper generation
semantics in one move — which is exactly what `67c5e048c` achieved for the
default build. Doing the same for synthetic-AQS mode means deciding what that
mode is still for; it exists for fake-JDK launchers with no real AQS bytecode
available, where "run the real bytecode" is not an option.

Two related gaps in the same native were fixed in
`docs/internal/fixed-suite-bugs/netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md`:
the `TimeUnit` ordinal was read from the wrong slot (so `await(30, SECONDS)`
waited 30 ms), and barrier failures were reported as `IllegalStateException`
instead of the declared checked exceptions.
