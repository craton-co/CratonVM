# A thread blocked in a native region lacks two C JVMTI answers

**Status: open. Filed 2026-10-10 by interpreter round i1 wave 46, lane L1
(the remainder of
`docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md`).
Both modes.**

Since wave 46 the C JVMTI table reads and writes another suspended thread's
locals and reads its monitors where its frames hold still: on the thread
itself while it is parked at a suspend point, or through its inspection
window while it is blocked in a native region (`Thread.sleep`,
`Object.wait`, `LockSupport.park`, blocking I/O). Two answers are still
missing for the blocked case. HotSpot 25.0.3 gives both (its handshake
reaches a thread in native code, which is at a safepoint).

## 1. `SetLocalObject` of a thread blocked in a native region

`jvmti::native_env::local_op_through_window` answers
`JVMTI_ERROR_NOT_AVAILABLE` (98) for a reference write; HotSpot answers
`JVMTI_ERROR_NONE` and the thread reads the new object when it wakes.
Primitive writes are served (`BlockedFrames::set_primitive_local`), and the
probe `tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.java`
shows one (`SetLocalInt waiter token=6`). A reference is not stored
because, while the thread sleeps, a collection folds its moves into the
thread's blocking deposit (`GcBlockState::slot_origins`), not into the
frames, and the wake writes each recorded slot's current address back
(`check_post_block_gc_refs`): a reference stored into a frame meanwhile is
neither a root nor remapped, and a recorded slot is overwritten at the wake.

**Evidence:** read from the code (`local_op_through_window`,
`LocalRead::Object(_) => return Err(ERR_NOT_AVAILABLE)`). The probe has no
row for it, so that its rows all match HotSpot.

**What would fix it:** the JDWP server's deferral
(`debug::inspect::defer_blocked_frame_write`, wave 41): record the write,
hold the object by a JDWP object id (collection disabled) or a JNI global
reference until the wake, and let the wake apply it after its write-back
(`inspect::apply_deferred_frame_writes_if_any`), which already runs for
JDWP's `StackFrame.SetValues`. The record would need a JVMTI-side variant
that does not require the JDWP snapshot (`DebugState::has_current_frames`),
which no C agent publishes.

## 2. A JNI native in the middle of a blocked thread's stack

A thread blocked in a native region is listed through its window from the
JDWP-shaped listing (`interpreter::read_blocked_rows`): the native it blocks
in on top, then its interpreter frames with the compiled activations. A JNI
native further down, one that called back into Java before the thread
blocked (`JvmThread::jni_native_frames`), is not listed; HotSpot lists it at
location -1. A thread parked at a suspend point lists it since wave 46 (it
runs the current thread's listing, `native_env::listed_rows`; the
`upcaller` rows of `L1W46JvmtiOtherThreadLocalsAndMonitors`). The same holds
for a reflective call's JDK frames (`JvmThread::reflective_calls`), which the
window listing leaves out too.

**Evidence:** read from the code (`read_blocked_rows` reads
`thread.frames`, `thread.running_native` and the compiled view, never
`jni_native_frames` or `reflective_calls`).

**What would fix it:** in `read_blocked_rows`, which holds the window, splice
the thread's `jni_native_frames` rows and `reflective_calls` frames by the
anchors their records carry (`(interpreter depth, JIT chain length)`), as
`listed_rows` does, against the blocked thread's recorded compiled view
(`GcBlockState::debugger_compiled`) instead of the reader's own JIT chain;
the top native already named by `blocked_native_top` must not be listed
twice. The JDWP listing (`publish_frame_snapshot`) would take the same rows
(`docs/known-issues/interpreter/i46-L1-proposal-one-frame-listing-for-jdwp-and-the-c-jvmti-table-20261010.md`).
