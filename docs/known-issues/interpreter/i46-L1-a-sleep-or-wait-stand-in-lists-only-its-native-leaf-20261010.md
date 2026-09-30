# A `Thread.sleep` / `Object.wait` stand-in lists only its native leaf

**Status: open (narrowed by the wave-46 follow-up, lane L1,
`claude/i46-L1b`). Filed 2026-10-10 by interpreter round i1 wave 46, lane
L1, from the orchestrator's JDI run of the merged wave-46 head
(`a03e7a9d5`).**

## What differed

This VM serves `Thread.sleep(long)` (and `sleep(long, int)`,
`sleep(Duration)`, `Object.wait()`, `wait(long)`, `wait(long, int)`) with a
registered native standing in for the JDK method, which runs no frame of
its own. A thread blocked or suspended in one listed its CALLER on top,
where HotSpot 25.0.3 lists the JDK frames and the native leaf:

| Scenario | HotSpot | CratonVM (`a03e7a9d5`) |
|---|---|---|
| `L1W45JdiSuspendedInNative`, all four modes | `sleeper top: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos java.lang.Thread.sleep`, `forceEarlyReturn` / `popFrames`: `NativeMethodException` | `sleeper top: L1W45JdiSuspendedInNative$Sleeper.run`, `forceEarlyReturn: ok`, `popFrames: InvalidStackFrameException` |
| `L1W43JdiForceEarlyReturn`, all modes | `top frame of a waiting thread is native: true` | `false` (the `parked` thread blocked in `Object.wait()` before the debugger attached) |
| `L1W43RawJdwpObjectErrorAnswers` (jdk-only, compatible, compatible:nojit) | `forceEarlyReturn(running other)`: 32 (`OPAQUE_FRAME`) | 34 |

## Fixed by the wave-46 follow-up

* **While a debugger is attached** (`debug::arm_blocking_standins`, the
  `METHOD_EVENTS_BLOCKING` gate), under `--jdk-only` a call of one of these
  methods (`debug::BLOCKING_STANDINS`) runs its JDK bytecode instead of the
  stand-in (`jvmti_events::run_blocking_standin`, from any caller,
  interpreted or compiled; not on a virtual thread), so the thread blocks
  in the genuine native leaf (`sleepNanos0`, `wait0`) under HotSpot's
  frames, and a suspension there parks at that native's return with the
  native on top (`park_if_suspended_at_native_exit`, wave 45).
  `CRATONVM_FRAME_TRACE=1` prints `[STOOD_IN_BLOCKING]
  java/lang/Thread.sleep(J)V` per such call.
* **A thread still in a stand-in** (it blocked before the debugger
  attached, or any thread under `--compatible`) lists the stand-in's
  native leaf on top (`jvmti_events::blocking_standin_leaf`, from the
  window listing, `interpreter::blocked_native_method`, and the native-exit
  park), so JDI sees a native frame and `ForceEarlyReturn` / `PopFrames`
  answer `OPAQUE_FRAME`, as on HotSpot.

## What remains

The JDK's Java levels between the leaf and the call (`sleepNanos`, `sleep`;
`wait(long)`, `wait()`) are not listed for a thread still in a stand-in:
they did not run. So under `--compatible` (whose stand-ins stay as they
were) `L1W45JdiSuspendedInNative` lists `sleeper top:
java.lang.Thread.sleepNanos0[native] L1W45JdiSuspendedInNative$Sleeper.run`
(predicted; `tools/jdi/known-differences.txt` carries the two lines for
the two `--compatible` modes), and a `--jdk-only` thread that blocked in
`Object.wait()` before the debugger attached lists `wait0` directly above
its caller.

**What would fix it:** list the census chain `stackwalker::native_standin_frames`
rebuilds from the real class bytes (each Java level at its call of the
next) between the leaf and the caller: the listing's native top
(`blocked_native_top`, `publish_frame_snapshot`'s `native_top`, the
native-exit park) would become a short list of rows, the Java levels
marked opaque (no locals) like compiled activations. Or run the stand-in's
bytecode under `--compatible` too, once `Thread.sleepNanos`'s JFR hooks
(`ThreadSleepEvent`) are shown to run there.

## Host result (orchestrator, 2026-09-30, `2f4ec7218`, `experimental-debug`)

`--jdk-only` (JIT and `--nojit`) matches HotSpot on every row of
`L1W45JdiSuspendedInNative`, and `L1W43JdiForceEarlyReturn` passed 20 of 20.
`--compatible` (both JIT modes) differs on three threads, not one:

```text
- sleeper top: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos java.lang.Thread.sleep
+ sleeper top: java.lang.Thread.sleepNanos0[native] L1W45JdiSuspendedInNative$Sleeper.run
- waiter top: java.lang.Object.wait0[native] java.lang.Object.wait L1W45JdiSuspendedInNative$Waiter.run
+ waiter top: java.lang.Object.wait0[native] L1W45JdiSuspendedInNative$Waiter.run
- parker top: jdk.internal.misc.Unsafe.park[native] java.util.concurrent.locks.LockSupport.parkNanos L1W45JdiSuspendedInNative$Parker.run
- parker forceEarlyReturn: NativeMethodException
- parker popFrames: NativeMethodException
+ parker top: L1W45JdiSuspendedInNative$Parker.run
+ parker forceEarlyReturn: ok
+ parker popFrames: InvalidStackFrameException
```

(The `top again` lines repeat the `top` lines.) The parker's
`LockSupport.parkNanos` / `Unsafe.park` stand-in is not among
`debug::BLOCKING_STANDINS`, so it gets neither the leaf row nor the JDK frames.

Since 2026-09-30 `--compatible` must match HotSpot too (`AGENTS.md`), so this is a
defect in both of its rows, not a known difference. The
`tools/jdi/known-differences.txt` entries for the sleeper lines are a
stopgap; remove them when this is fixed. The fix is the page's own
proposal, extended to the waiter and to `LockSupport.park*` / `Unsafe.park`:
run the JDK bytecode down to the leaf native while a debugger is attached,
in both modes, or list the JDK levels the stand-in skipped.
