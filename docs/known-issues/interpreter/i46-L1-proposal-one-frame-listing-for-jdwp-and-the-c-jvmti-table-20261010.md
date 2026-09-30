# Proposal: one frame listing for JDWP and the C JVMTI table

**Status: open. Filed 2026-10-10 by interpreter round i1 wave 46, lane L1
(proposal, kept for triage).**

## Why

A thread's stack is listed by three builders that each know part of what
HotSpot lists:

| Builder | Used by | Knows |
|---|---|---|
| `native_env::listed_rows` (the thread lists itself) | C JVMTI table: the current thread, and since wave 46 a thread parked at a suspend point | interpreter frames, compiled activations, JNI natives in the middle (`jni_native_frames`), a registered native at a native-exit park, reflective calls' JDK frames (`reflective_calls`, wave 46) |
| `interpreter::publish_frame_snapshot` | JDWP `ThreadReference.Frames` / `FrameCount` (parked and blocked threads) | interpreter frames, compiled activations, the native on top |
| `interpreter::read_blocked_rows` | C JVMTI table for a thread blocked in a native region | as the JDWP snapshot |

Every round-i1 wave that taught one of them a kind of frame left the others
behind: wave 42's JNI native rows and wave 46's reflective frames reach only
the first. So a debugger (JDWP) and a C agent list different stacks for one
thread, and a C agent lists different stacks for one thread depending on
whether it sleeps or spins:

* JDWP lists no JNI native in the middle of a stack, and no
  `Method.invoke` / `DirectMethodHandleAccessor.invoke` frames (item 4 of
  `docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`);
  HotSpot's back end lists both, since it reads JVMTI.
* The C table lists neither for a blocked thread
  (`docs/known-issues/interpreter/i46-L1-a-thread-blocked-in-a-native-lacks-two-c-jvmti-answers-20261010.md`).
* Depths differ between the builders, so a JDWP frame id and a JVMTI depth
  name different frames once a middle native is involved.

## Design sketch

1. A listing type, `FrameListing` (rows top first: an interpreter frame
   with its index and whether its body runs compiled, a compiled or inlined
   activation, a native method, a spliced JDK frame), built by one function
   from a thread's `frames`, `jni_native_frames`, `reflective_calls` and a
   compiled view: the thread's own (`capture_blocked_compiled_view` on the
   thread, as `listed_rows` does through
   `stackwalker::capture_trace_with_anchor_positions`) or the one it
   recorded as it blocked (`GcBlockState::debugger_compiled`).
2. `listed_rows` becomes that function over the thread's own view;
   `read_blocked_rows` the same over the recorded one (the window already
   holds the thread still, and `jni_native_frames` / `reflective_calls` are
   not touched while it is blocked).
3. `publish_frame_snapshot` builds its `FrameEntry` list from the listing:
   the frame ids count every row, and `DebugState::opaque_frames` marks the
   rows without locals (natives, compiled activations, spliced JDK frames),
   as it marks compiled activations now; the local-variable readers already
   skip them (`is_interpreter_row`).
4. `GetAllStackTraces` (wave 46) can then take a VM-wide snapshot: suspend
   every thread with one handshake each (`DebugState::begin_handshake`), wait
   for all to stand still, list all, release all, as HotSpot takes all stacks
   at one safepoint.

## Evidence to gather first

The JDI scenario of `L1W46JvmtiOtherThreadLocalsAndMonitors`' `upcaller`
(a JNI native calling back into Java) and of `L1W46JvmtiReflectiveFrames`
(a target called through `Method.invoke`), with `ThreadReference.frames()`
printed, against HotSpot's back end: the rows the JDWP listing lacks today.

## Cost

Debugger-only: the listing is built when a debugger or an agent asks, or
when a suspended thread parks (as the snapshot is now). Nothing on any
per-bytecode or per-call path; `jni_native_frames` and `reflective_calls`
are already recorded.
