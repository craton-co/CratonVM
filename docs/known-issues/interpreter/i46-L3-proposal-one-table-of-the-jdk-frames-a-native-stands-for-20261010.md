# Proposal: one table of the JDK frames a native stands for

**Status: proposal — filed 2026-10-10 by interpreter round i1 wave 46, lane
L3 (the direction the round's stack-walk work points at). Not built.**

## Where the round left it

A native that serves a JDK method in place of its bytecode pushes none of the
JDK frames HotSpot shows for it. Waves 13 to 46 taught the captures to list
those frames anyway, one mechanism per native family, each with its own
table, placement rule and memo:

| Family | Table | Where the frame stands | Consumers |
|---|---|---|---|
| `Thread.sleep`, `Object.wait` (round 13) | `stackwalker::native_standin_frames` census | the leaf native's caller chain | throwables only |
| `Method.invoke`, `Constructor.newInstance` (wave 43) | `METHOD_INVOKE_STEPS`, `CONSTRUCTOR_NEW_INSTANCE_STEPS` | each step at its call of the next (`first_call_to`) | throwables, `Thread.getStackTrace`, walks, published traces (wave 45) |
| a native-raised `InvocationTargetException` (wave 45) | `reflective_raise_entries` | the accessor's nth `new` of the class, by `catch` arm | throwables |
| a failed argument check (wave 46) | `reflective_check_entries`, `null_unbox_frame` | a check method at its `new`'s constructor call; `ValueConversions.unbox*` at its `<x>Value()` call | throwables |
| a JNI native's own row (wave 42/44) | `capture_trace_with_anchor_positions` | the native at its anchor | JVMTI `GetStackTrace` |

Each family re-derives the same three things: which JDK methods the native
stands for (by class, name and descriptor), which instruction of each the
frame stands at (a call, a `new`, a constructor call, found in the running
image's bytes so the line is the image's own), and where the frames go in a
capture (below the first slot the call pushed, or on top for a throwable the
native raised). What is still missing is the same list again for each new
consumer: JVMTI and JDWP do not list the reflective frames (item 4 of
`docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`),
the round-13 stand-in frames are listed for a throwable only (read from
the code: `append_native_standin_frames` runs in
`capture_throwable_stack_trace` alone; whether HotSpot's walks and
thread dumps of a sleeping thread need them is not measured), and a `SHOW_HIDDEN_FRAMES` walk lists none of the
method-handle frames HotSpot runs for `invokeExact` or `Method.invoke`
(`tools/probes/interp/L3/L3W46InvokeFramesInWalks.java`, the two
`*-hidden-shows-invoke-frames` rows).

## The proposal

One declarative record per native, registered next to the native itself:

```text
StandIn {
    frames: [ (class, method, descriptor, at: CallOf(owner, name, desc) | NthNew(class, n) | CtorCallOfNthNew(class, n) | Native) ],   // outermost first
    raises: [ (throwable class | check, frames) ],                                                                                         // on-top variants
    hidden: bool,                                                                                                                          // listed only when the view shows hidden frames
}
```

* **Resolved once per image and redefinition count**, as
  `JvmThread::reflective_frames_named` is today, into ready
  `StackTraceEntry`s; resolution is the only step that reads bytes or takes
  the class-manager lock, so a published trace (which may not take it) and a
  JVMTI read of another thread can list the frames from the memo.
* **Recorded on entry** by one call the registry makes for every native that
  has a `StandIn` (the reflective record generalised: kind = the native's
  registry index, plus the failed check the native notes), so no native
  family needs its own `enter_*` / `leave_*` pair.
* **Placed by one rule** (`trace_anchor_position`, and on top for a throwable
  the native raised), shared by every capture, the published trace and
  `native_env::frames_of`.

It would retire the four per-family tables above, give JVMTI / JDWP the
reflective frames without lane L1 re-deriving them, and let a
`SHOW_HIDDEN_FRAMES` walk list the `DirectMethodHandle$Holder` /
`Invokers$Holder` frames HotSpot shows for a method-handle call by marking
them `hidden`, at no cost to the call itself (the record is the per-call cost
the reflective path already pays: one push and one truncate).

## What it must not do

* Add a per-call cost to a native without a `StandIn` (the registry's
  dispatch must decide by a bit in the native's entry, not a lookup).
* List a frame whose method it cannot find: every family is fail-closed
  today (nothing listed) and must stay so, JDK 17 images included.
* Change `--compatible` output except where it is a HotSpot divergence a
  probe shows.

## First step

Move `native_standin_frames` and the reflective steps behind one
`StandIn` table in `stackwalker.rs` with the existing probes as the gate
(`L3W43ReflectionFrames`, `L3W45ReflectiveRaise`,
`L3W45PublishedReflectiveTrace`, `L3W46ReflectiveChecks`, the round-13
stand-in probes): byte-for-byte the same rows, before any new consumer.
