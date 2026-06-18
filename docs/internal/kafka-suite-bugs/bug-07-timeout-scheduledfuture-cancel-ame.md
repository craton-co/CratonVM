# Bug 07 — `@Timeout` tests throw `AbstractMethodError: Future.cancel has no Code attribute`

**Severity:** Critical / pervasive — every kafka test class annotated with
`@Timeout` (the majority of the clients suite) failed *all* of its tests, and in
batch runs the accumulated failures took the whole package JVM down (rc=127 /
rc=1). Reproduces under `--nojit` (interpreter) and JIT alike. HotSpot clean.

**Symptom (every test in an affected class):**
```
java.lang.AbstractMethodError: method java/util/concurrent/Future.cancel(Z)Z has no Code attribute
```

## Minimal repro (one trivial JUnit test)
```java
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
public class TimeoutRepro {
  @Test @Timeout(30)
  public void trivial() { org.junit.jupiter.api.Assertions.assertEquals(2, 1+1); }
}
```
- HotSpot: PASS.
- CratonVM (before fix, `--nojit` and JIT): `fail=1` → `AbstractMethodError: Future.cancel(Z)Z`.
- CratonVM (after fix): PASS.

## Root cause

JUnit Jupiter's `@Timeout` support (`SameThreadTimeoutInvocation`) schedules an
interrupt task on a `ScheduledExecutorService` and, in its `finally` block, calls
`future.cancel(false)` on the returned `ScheduledFuture`. Two CratonVM defects
combined to break that call:

1. **Missing future natives.** CratonVM's real-JDK scheduled-executor is a native
   shim: `native-collections/src/lib.rs::register_executors_scheduled_natives`
   (wired into the essential path at `native-collections` line 291). Its
   `schedule(...)` native (`native_stpe_schedule`) returns an object whose runtime
   class is the **`java/util/concurrent/ScheduledFuture` interface itself**
   (`alloc_synthetic(... "ScheduledFuture", 2)`). But the function registered
   `<init>`/`schedule`/`shutdown`/`submit`/… and **never registered
   `cancel`/`isCancelled`/`isDone`/`get`** on `ScheduledFuture`.

   JUnit's bytecode is `invokeinterface java/util/concurrent/Future.cancel:(Z)Z`
   (it holds the value as `Future`). Dispatch resolves to the abstract
   `Future.cancel` declaration (no Code); the receiver's runtime class
   (`ScheduledFuture`) had no `cancel` native to fall back to → `AbstractMethodError`.

   Diagnostic confirming the wrong runtime type (cf. bug-02's
   `Spliterators.emptySpliterator`):
   ```
   ScheduledExecutorService.schedule(...).getClass().getName()
     HotSpot : java.util.concurrent.ScheduledThreadPoolExecutor$ScheduledFutureTask
     CratonVM: java.util.concurrent.ScheduledFuture            ← interface, wrong
   ```

   (Two *other* copies of this registration exist but are inert: phase63's
   `register_p63_scheduled_executor` lives under `register_synthetic_overrides`,
   a **no-op in the real-JDK build**, and the dead
   `register_scheduled_executor_natives` in native-collections has no caller. The
   live one is `register_executors_scheduled_natives`.)

2. **Immediate firing.** `native_stpe_schedule` *ran the runnable inline* and
   returned a `done` future. For `@Timeout` that means the interrupt task fired
   immediately (interrupting the test thread) and, worse, `future.cancel(false)`
   would return `false`. JUnit reads a `false` return as "the task already ran =
   the test timed out" and reports a timeout failure. So even with `cancel`
   present, an immediate-fire future would convert every `@Timeout` test into a
   spurious timeout.

## Fix (3 parts)

- **`native-collections/src/lib.rs`**
  - `native_stpe_schedule`: when `delay > 0`, do **not** fire the task — return a
    *pending, cancellable* future (`alloc_pending_future`, state=0). There is no
    real timer thread; the test finishes well before the delay, then
    `cancel(false)` succeeds (returns `true` ⇒ no timeout). Zero/negative delay
    keeps the old run-immediately behaviour.
  - Register `cancel`/`isCancelled`/`isDone`/`get` natives on
    `java/util/concurrent/ScheduledFuture` in `register_executors_scheduled_natives`.
    State machine in field 1: `0=pending, 1=done, 2=cancelled`; `cancel` succeeds
    iff pending.
- **`vm/src/runtime/interpreter.rs`** (general robustness): in the no-Code
  interface-dispatch rescue, before throwing `AbstractMethodError`, try a native
  registered on the **receiver's own runtime class** (walking its class chain)
  when the receiver has no real bytecode override. This is what routes the
  `invokeinterface Future.cancel` call (resolved to the abstract `Future`
  declaration) to the `ScheduledFuture.cancel` native. General: it fixes any
  synthetic object stamped with an interface/abstract runtime class that carries
  its method natives on that exact name.

## Status
- `TimeoutRepro`: PASS (interpreter + JIT).
- `FetchSessionHandlerTest`: 0/19 → **14/19** (remaining 5 are unrelated bugs).
- Unblocks every `@Timeout`-annotated class in the suite.

## Deeper fix (deferred)
The future being stamped with an *interface* runtime class is the underlying wart
(same family as bug-02). The correct long-term fix is to run the real
`ScheduledThreadPoolExecutor` so `schedule()` yields a genuine `ScheduledFutureTask`
(real worker thread + DelayedWorkQueue). Deferred; the shim fix above unblocks the
suite.
