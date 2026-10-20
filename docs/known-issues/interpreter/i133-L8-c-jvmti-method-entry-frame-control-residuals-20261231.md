# C JVMTI frame control from a `MethodEntry` callback: two residual shapes

**Status: OPEN (not built, not measured). Filed 2026-12-31 by interpreter round i6 wave 133, lane
L8**. These are the residuals of
`docs/internal/proposals/interpreter/i62-L1-proposal-one-agent-request-word-the-dispatch-loop-reads-20261026.md`
(DONE: `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` has been ON since wave 129). Both were recorded
open in that page's waves 95 and 115. The neighbouring case, a thread *blocked* on a monitor
entry and suspended, is `i133-L1-c-jvmti-frame-control-of-a-thread-blocked-on-monitor-entry`.

## The two shapes (by reading)

1. **A contended `synchronized` method entry.** When the monitor is contended, the `MethodEntry`
   event is posted once the monitor is held: `vm/src/vm/vm_exec.rs` (around the call of
   `interpreter::fire_deferred_method_entry`) and
   `vm/src/runtime/interpreter/jvmti_events.rs` `fire_deferred_method_entry`. That post is not
   wrapped in a callback point. So a `ForceEarlyReturn*` or `PopFrame` made in that callback is
   not recorded for the loop. The uncontended entry goes through `fire_method_entry_after_push*`
   and is served.
2. **The entry frame of a nested loop.** These are frames pushed by a JNI upcall, by reflection,
   or by `execute_prebuilt_frame`. These pushes reach `fire_method_entry_after_push_with` with
   no `in_loop` VM, so its `_` arm posts the event outside any callback point. The request keeps
   `OPAQUE_FRAME`, as wave 115 recorded.

HotSpot's answer for each is not measured. By reading `JvmtiEnvBase` (`force_early_return`,
`UpdateForPopTopFrameClosure`), HotSpot serves a Java top frame and answers `OPAQUE_FRAME` only
when the caller is native. On that reading, shape 2's `PopFrame` may already be HotSpot's answer
when the caller is a JNI native, while shape 2's `ForceEarlyReturn*` and shape 1 would differ.

## What a fix needs

1. Probe rows first, MEASURED on HotSpot 25.0.3 with the MSVC-built shim, default flags and
   `-Xint`. Add two rows to `tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.java` / `.c`:
   * `entry-contended`: a `synchronized static int` entered while another thread holds the class
     monitor, with `ForceEarlyReturnInt` and `PopFrame` from the callback;
   * `entry-jni`: the same method called through `CallStaticIntMethod` from a native.
2. Shape 1: wrap the deferred post in the same callback point as the uncontended entry. Set the
   request word's bit for the frame the loop is about to run.
3. Shape 2: only where the probe shows HotSpot serves it. A forced return of the nested loop's
   entry frame would have to unwind the nested `execute`.
4. Put both behind a parity switch (default ON with `=0` once the rows match HotSpot in both
   modes, JIT and `--nojit`).

## Wave 134 (lane L1), 2026-12-31

**Status: OPEN. Shape 1 code DONE (not built, not measured). Shape 2 not served.**

Shape 1 (contended `synchronized` entry): the deferred `MethodEntry` of a frame that an
interpreter invoke pushed for its caller's loop is now posted at the same callback point as
the uncontended entry (`interpreter::fire_deferred_method_entry_in_loop`, called from
`vm_exec::enter_synchronized_method_in_pushed_frame`). A `ForceEarlyReturn*` or `PopFrame`
made in that callback is therefore recorded and taken before bytecode 0. The frame already
owns the monitor and releases it with the frame. The parity switch is
`CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL` (`VmConfig::jvmti_contended_entry_frame_control`,
default ON, `=0` posts outside a callback point as before). It needs
`CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`. `interpreter::execute`'s deferred entry (the entry
frame of a nested loop) now calls `vm_exec::enter_synchronized_method_in_pushed_entry_frame`,
which keeps the old post (shape 2).

Shape 2 (the entry frame of a nested loop: JNI upcall, reflection, `execute_prebuilt_frame`):
unchanged, `OPAQUE_FRAME`. By reading, HotSpot serves the forced return there and refuses the
pop (native caller). The probe rows below measure it. Serving the forced return would need
the nested `execute` to return the value from its entry frame at the callback point. That is
not done.

Probe (new, shared with i133-L1): `tools/probes/interp/L1/L1W134JvmtiBlockedEntryFrameControl.java`
+ `.c`. Rows: `entry contended int`, `entry contended pop`, the control `entry uncontended int`,
and `entry jni int` / `entry jni pop` (the `entry-jni` row this page asks for). Its
HOTSPOT_EXPECTED block is from reading, NOT MEASURED. The rows were not added to
`L1W62JvmtiCallbackFrameControl`: that probe arms the current thread only, and a contended
entry needs a second thread.

Orchestrator:

1. The unit tests of the i133-L1 page's step 1 (`i134_l1_contended_entry_tests` covers this
   page).
2. Record HotSpot 25.0.3 (default flags and `-Xint`) with the shim and write the measured
   block.
3. Debug `cratonvm`, `--jdk-only` and `--compatible`, JIT and `--nojit`. Pass:
   `entry contended int: int:46=0 hits=1 calls=0 returned 46`,
   `entry contended pop: pop=0 hits=2 calls=1 returned 109` and `entry uncontended int` equal
   HotSpot's measured lines. Negative control: `CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL=0`
   gives `int:46=32 hits=1 calls=1 returned 109` and `pop=32 hits=1 calls=1 returned 109`.
   Expected to differ (shape 2, OPEN): `entry jni int: int:47=32 hits=1 calls=1 returned 9`.
   `entry jni pop` matches HotSpot if HotSpot measures 32.
4. If shape 1 does not match HotSpot's MEASURED line, flip
   `CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL` to default OFF and keep the switch.
