# Proposal: the C JVMTI table's next functions — current thread, stack, lines

**Status: open — filed 2026-09-25 by interpreter round i1 wave 13, lane L1
(proposal; kept for triage).**

## Why

After wave 13 a native agent built against `jvmti.h` can negotiate
capabilities, receive `VMInit` / `VMDeath` / `ClassPrepare` / `Exception` /
`Breakpoint`, find a class's methods (`GetClassSignature`,
`GetClassMethods`, `GetMethodName`) and set breakpoints
(`vm/src/jvmti/native_env.rs`). What the common tracer and coverage agents do
next in their callbacks answers `JVMTI_ERROR_NOT_AVAILABLE`:

* `GetLineNumberTable` (70) — every line-breakpoint agent maps a source line
  to a `jlocation` before `SetBreakpoint`, and maps a `Breakpoint`'s location
  back to a line;
* `GetCurrentThread` (18), `GetFrameCount` (16), `GetStackTrace` (104),
  `GetFrameLocation` (19) — the "where am I" of a `Breakpoint` / `Exception`
  callback;
* `GetMethodModifiers` (66), `IsMethodNative` (76), `GetMethodLocation` (71),
  `GetClassStatus` (49), `GetLoadedClasses` (78) — filtering and the
  attach-time class scan.

All of them read state the VM already exposes to JDWP: `debug/inspect.rs`
and the dispatch loop's frame publication read line tables and frames, and
`runtime::jvmti::resolve_method_id_for_vm` / `jni::decode_method_id` map
`jmethodID`s.

## Design

1. **Class and method facts** (49, 66, 70, 71, 76, 78): pure reads of the
   class manager behind `method_of` / `class_of`, answering `Allocate`
   blocks as `GetClassMethods` does. `GetLineNumberTable` reads the method's
   `LineNumberTable` attribute (the same decode `interpreter::debugger_line_info`
   uses) and answers `ABSENT_INFORMATION` without one. Capability-gated where
   the spec says so (`can_get_line_numbers`, added to `POTENTIAL` in the same
   change).
2. **Current thread and its stack** (16, 18, 19, 104): only for the calling
   thread inside a callback at first (`in_event_context` already installs
   the thread's `JvmThread`), reading `thread.frames` top-down with each
   frame's `last_instr_pc` (the top frame's current pc is the event's
   location). Another thread's stack needs it suspended, which the C table
   does not offer yet (`SuspendThread` is not in the table); answer
   `THREAD_NOT_SUSPENDED` for any other thread.
3. **Compiled frames**: a thread in a callback is in the interpreter (every
   delivered event is posted by the interpreter), so step 2 needs no
   compiled-frame walk; a later `GetStackTrace` of a suspended thread would
   reuse the JDWP publication (`publish_debugger_frames`).

## Staged plan

1. Step 1 with unit tests through the table, beside
   `jvmti::native_env::tests` (a synthetic class with a `LineNumberTable`).
2. Step 2, tested from a `Breakpoint` callback of the ten-trip loop fixture
   (frame count, the location, the method id of each frame).
3. The C-agent driver of
   `docs/internal/fixed-bugs/interpreter-L1-proposal-jdi-conformance-harness-FIXED-20261003.md`
   prints a line-breakpoint transcript on HotSpot and CratonVM.

## Expected benefit

The minimum a line-breakpoint or exception-tracing agent needs, so agents
written for HotSpot (coverage probes, `Exception` loggers, simple tracers)
run unchanged in debug builds. No cost without an agent: every function is a
table slot reached only by agent calls.

## Progress (wave 17)

Interpreter round i1, wave 17, lane L2 landed most of stage 1 (the class and
method facts) in `vm/src/jvmti/native_env.rs`:

* slot 49 `GetClassStatus` (`class_status`: HotSpot's `jvmti_class_status`
  — an array class `ARRAY`, a primitive `PRIMITIVE`, any other class JDWP's
  bits from `debug::jdwp_class_status`, now `pub(crate)`);
* slot 66 `GetMethodModifiers` (`JVM_RECOGNIZED_METHOD_MODIFIERS`, 0x1DFF);
* slot 76 `IsMethodNative`;
* slot 71 `GetMethodLocation` (`0` .. last bytecode index; `-1` / `-1` for a
  method without code; `JVMTI_ERROR_NATIVE_METHOD` for a native one);
* slot 70 `GetLineNumberTable` behind `can_get_line_numbers` (added to
  `POTENTIAL`): the `LineNumberTable` rows by start location as
  `jvmtiLineNumberEntry` (16 bytes) in an `Allocate` block,
  `JVMTI_ERROR_ABSENT_INFORMATION` without a table, `NATIVE_METHOD` for a
  native method.
* `with_method` is the one `jmethodID` decode (`method_of` uses it).

Test: `jvmti::native_env::tests::the_c_table_answers_class_status_method_facts_and_line_numbers`
(a fabricated class's status 7, the modifiers of a static and a native
method, `IsMethodNative`, the loop method's `(0, 23)`, the line table
without and with the capability, the errors for a native method, a method
without a table, a bad id, a NULL out-parameter, a disposed env).

Left for stage 1: `GetLoadedClasses` (78) — decide first whether hidden
classes and the VM's own synthetic classes (lambda proxies,
`cratonvm/synthetic/*`) are listed, as HotSpot lists hidden classes but
has no synthetic carriers. Stage 2 (current thread and its stack) is
unchanged; the running-native record of
`i15-L1-proposal-record-the-running-native-on-the-thread-20260925.md`
(wave 17, stage 1) gives `GetFrameLocation` / `GetStackTrace` a native top
frame once they exist.

## Progress (wave 22)

Interpreter round i1 wave 22, lane L1: **stage 2 landed** in
`vm/src/jvmti/native_env.rs`, for the current thread:

* slot 18 `GetCurrentThread` (start or live phase): the calling thread's
  `java.lang.Thread` as a local reference (NULL while it has none), read
  inside the foreign-thread entry as `thread_id_of` reads a mirror;
* slot 16 `GetFrameCount`, slot 19 `GetFrameLocation` (depth 0 is the top,
  `JVMTI_ERROR_NO_MORE_FRAMES` past the bottom, `ILLEGAL_ARGUMENT` for a
  negative depth), slot 104 `GetStackTrace` (a negative `start_depth` counts
  from the bottom; `ILLEGAL_ARGUMENT` for a positive one at or past the depth,
  a negative one below `-depth`, a negative `max_frame_count`), live phase
  only. The frames are the thread's interpreter frames at their
  `last_instr_pc` (the top one's is the bytecode an event is posted for) with
  the compiled activations and their inlined callees spliced in exactly as a
  stack trace splices them (`stackwalker::capture_full_trace_without_store`),
  each as `(jmethodID, jlocation)`.
* The current thread is found through the JNI layer
  (`jni::current_jvm_thread_of`: the installed `JvmThread` when the VM's
  registry publishes it, else the registered thread of this OS thread); a
  thread the VM does not know is `JVMTI_ERROR_UNATTACHED_THREAD`, and a
  non-NULL `jthread` naming ANOTHER thread is `JVMTI_ERROR_NOT_AVAILABLE`
  (its frames may be read only while it is suspended, and the table has no
  `SuspendThread`).

Test: `jvmti::native_env::tests::the_c_table_answers_the_current_threads_stack_in_a_callback`
(from a `Breakpoint` callback of the ten-trip loop: one frame, the loop
method at 9, and each specified error; an unknown OS thread is
`UNATTACHED_THREAD`).

Still open: another thread's stack (needs `SuspendThread` /
`ResumeThread` in the table, then the JDWP publication's frames:
`publish_debugger_frames` / the blocked-thread window), a native method's own
frame on the stack (the interpreter pushes no frame for a native; the
running-native record of
`i15-L1-proposal-record-the-running-native-on-the-thread-20260925.md` could
name the innermost one), and `GetLoadedClasses` (78) from stage 1.

## Progress (wave 44) — lane L1

Interpreter round i1 wave 44, lane L1 added the local-variable and monitor
functions, for the current thread, and checked them against HotSpot 25.0.3
with a C-agent probe (`tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java`
and `.c`, run with `-agentpath`):

* slots 21-30, `GetLocal{Object,Int,Long,Float,Double}` and
  `SetLocal{Object,Int,Long,Float,Double}`, and slot 155,
  `GetLocalInstance`;
* slot 11, `GetCurrentContendedMonitor` (of any thread);
* slot 153, `GetOwnedMonitorStackDepthInfo`.

Their capabilities (`can_access_local_variables`,
`can_get_current_contended_monitor`,
`can_get_owned_monitor_stack_depth_info`) are potential now.

Each error code is HotSpot's, in HotSpot's order: `MUST_POSSESS_CAPABILITY`
without the capability, and for the local functions `ILLEGAL_ARGUMENT`,
`NULL_POINTER`, `THREAD_NOT_SUSPENDED`, `NO_MORE_FRAMES`, `OPAQUE_FRAME`,
`INVALID_SLOT` and `TYPE_MISMATCH`. The slot checks are
`native_env::checked_local`.

The probe also covers `GetFrameLocation` and `GetStackTrace` with negative
start depths. They already matched HotSpot.

Still open: another thread's stack. See
`docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md`.
