# Proposal: record the native method a thread is running, for JDWP and JVMTI

**Status: open — filed 2026-09-25 by interpreter round i1 wave 15, lane L1
(proposal, kept for triage).**

## Why

Wave 15 lists the native method a suspended, blocked thread is in
(`interpreter::blocked_native_method`, see
`docs/internal/fixed-bugs/interpreter-L1-jdwp-suspension-does-not-reach-compiled-or-native-code-FIXED-20261005.md`,
"Progress (wave 15)") by decoding the invoke at the caller frame's
`last_instr_pc`. That is exact only when no override can replace the
resolved method, so it leaves out:

* a virtual or interface call to an overridable native (the receiver, and
  with it the selected method, is gone when the thread blocks);
* a native entered from compiled code (the top interpreter frame's invoke is
  the compiled method's);
* `StackFrame.ThisObject` of an instance native through a raw JDWP client
  (HotSpot answers the receiver);
* JVMTI `GetFrameLocation` / `GetStackTrace` from the C table (proposal
  `i13-L1-proposal-jvmti-c-table-stack-and-line-functions`), which need the
  same top frame.

## Design

A per-thread record, written by the doors that call a native and read only
by debugger code:

1. `JvmThread::running_native: Cell<(u32 class id, u32 method index)>`
   (plain data, no `ObjectRef`, so it needs no entry in the per-thread root
   list), set to the SELECTED method by the native call doors
   (`invoke_cached_native_callback*`, the by-name routes in `vm_exec.rs`,
   the JIT's native site helper) around the callback, and restored after it
   (natives nest through JNI upcalls).
2. The receiver: the doors already pin native arguments across the call
   (`InvokeArgsRootGuard` / `native_pin_roots`); record the index of the
   receiver's pin in the same cell so the blocked-thread reader can export
   it through the inspection window like any deposited reference.
3. `blocked_native_method` reads the cell first and falls back to the
   decode.

## Cost and staging

One store before and one after each native call — measure on the native-call
microbenchmarks (`L4` probes) before landing; if it shows, write it only
while `DebuggerGates::session_attached` or a JVMTI env exists (one load,
which the doors already pay for `native_sync_enabled`-style facts).

1. Stage 1: the cell for the interpreter's native doors, read by
   `blocked_native_method` (closes the virtual-native gap).
2. Stage 2: the receiver pin index (closes `ThisObject`).
3. Stage 3: the JIT native site helper (closes natives entered from compiled
   code) — jit-round coordination.

## Verify

jdb against a program blocked in `FileInputStream.read()` (an overridable
public method that calls a private native) and in a user class's overridable
native loaded through JNI: `where` lists the native first, as on HotSpot.

## Progress (wave 17)

Interpreter round i1, wave 17, lane L2 landed stage 1, in a cheaper shape
than the design above: the record is the native **callback address**, set in
the one funnel every non-leaf native call passes (interpreter doors, the
by-name routes, and compiled code's non-leaf native calls), not a
`(class id, method index)` threaded through every door.

* `JvmThread::running_native: usize` (plain data, in neither root list;
  `vm/src/threading/jvm_thread.rs`).
* `interpreter::enter_running_native` / `leave_running_native`
  (`vm/src/runtime/interpreter.rs`): `vm_exec::safe_native_call_impl` sets
  the field around the callback's `catch_unwind` while
  `DebuggerGates::session_attached()` holds and restores the prior value
  after it (natives nest through upcalls). Leaf natives
  (`safe_native_call_leaf`) are not recorded: they cannot block.
* `interpreter::blocked_native_method(shared, top, running_native)`: the
  fixed-target decode is unchanged; for a virtual or interface call to an
  overridable method it now names the `ACC_NATIVE` instance method of the
  invoke's name and descriptor, declared by the resolved class or a loaded
  subtype of the symbolic owner (`ClassStore::subtypes_of`, at most
  `BLOCKED_NATIVE_MAX_SUBTYPES` = 256), whose registered callback
  (`NativeMethodRegistry::find`) is the recorded one; none or two → `None`
  as before. `read_blocked_frames` and `publish_blocked_frames_if_suspended`
  pass the blocked thread's record.

Tests (`interpreter::i1w10_l1_debugger_event_tests`, `experimental-debug`):
`a_virtual_call_to_an_overridable_native_is_named_from_the_running_callback`
(base native, subclass override, no record, unrelated callback) and
`the_running_native_is_recorded_only_while_a_debugger_is_attached`
(unarmed: nothing written; nested enter/leave restores the outer record).

Cost: unarmed, one `Acquire` load of the session gate and a not-taken branch
per non-leaf native call, inside a funnel measured at ~26-36 ns
(`native_funnel_profile`); no store. For the orchestrator to time: the L4
native-call probes and `probes/NativeShapeProbe.java` under `--nojit`
(non-leaf rows, e.g. a `System.identityHashCode` or `Object.hashCode` loop),
interleaved against the wave-16 build; the expected difference is below the
noise floor.

What stage 1 does not close, and the next stages:

* A JNI-bound user native (`System.loadLibrary` + `RegisterNatives` / symbol
  lookup) whose callback the registry does not hold under the candidate's
  triple is not matched (stage 1b: record the JNI bridge's method identity,
  or ask the JNI binding table by callback).
* A class whose subtypes exceed the cap (`Object`, widely implemented JDK
  interfaces) is matched only by its own declaration; an override in a
  subclass that registers the same Rust function under the same triple would
  be listed as the superclass's method.
* Stage 2 (the receiver pin index, for `StackFrame.ThisObject`) and stage 3
  (a native entered from compiled code: the record is already set there,
  but the top interpreter frame's invoke is the compiled method's, so the
  reader needs the native's own identity, e.g. a reverse registry lookup by
  callback) are unchanged.
* The C JVMTI table's `GetFrameLocation` / `GetStackTrace` (proposal
  `i13-L1-proposal-jvmti-c-table-stack-and-line-functions`) can use the same
  reader once they exist.

## Wave 22 note

Interpreter round i1 wave 22, lane L1: **stage 3 for the JDWP listing,
landed without a reverse registry lookup.** Since wave 21 a blocked thread
records its compiled activations, and the innermost one's bytecode index is
the invoke that entered the native. `interpreter::blocked_native_top` decodes
that invoke (`native_above_compiled_row`, the decode shared with
`blocked_native_method` as `native_named_by_invoke`) and names its native
only when the running-native record is the resolved method's registered
callback — required even for a fixed target, because the compiled frame's
bci is the last safepoint id it published. Test:
`interpreter::i1w10_l1_debugger_event_tests::a_native_called_from_compiled_code_is_listed_above_it`.
Still open: stage 1b (a JNI-bound native), stage 2 (the receiver for
`ThisObject`), and the C table's stack functions
(`i13-L1-proposal-jvmti-c-table-stack-and-line-functions-20260925.md`).

## Progress (wave 42) — lane L1

The defect fix
`docs/internal/fixed-bugs/interpreter-L1-jvmti-stack-functions-omit-the-native-method-that-calls-them-FIXED-20261006.md`
built the JNI half of this record for the C JVMTI table: every JNI native
call (both JNI arms of `vm_exec::invoke_on_class_shared_inner`) pushes
`(interpreter depth, declaring class, JNI function pointer)` onto
`JvmThread::jni_native_frames` (`jvmti::native_env::NativeFrameRow`),
unconditionally, and `jvmti::native_env::frames_of` lists those natives at
location -1. Stage 1b of this page (naming a JNI-bound native for the JDWP
blocked-thread listing, `interpreter::blocked_native_top`) could read the
same list: its innermost row names the JNI native exactly, where the
`running_native` callback record names only registered natives. Not wired in
wave 42.
