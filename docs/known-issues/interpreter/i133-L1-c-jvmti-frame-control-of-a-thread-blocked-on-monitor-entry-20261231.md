# C JVMTI `PopFrame` / `ForceEarlyReturn*` of a thread blocked on a monitor entry answer `OPAQUE_FRAME`

**Status: OPEN (not built). Filed 2026-12-31 by interpreter round i6 wave 133, lane L1**, as the
residual items 2 and 3 of
`docs/internal/proposals/interpreter/i61-L1-proposal-frame-control-from-an-event-callback-20261025.md`,
whose stages 1 and 2 are DONE (that page's "Wave 133 (lane L1)").

## Symptom (by reading; not yet MEASURED)

A C agent suspends a thread that is blocked entering a monitor (`monitorenter` on a lock another
thread holds, or the entry of a `synchronized` method) and calls `PopFrame` or
`ForceEarlyReturn*` on it.

- HotSpot (by reading `JvmtiEnvBase::force_early_return` and `UpdateForPopTopFrameClosure`, as
  the i61 page does): the top frame is the Java frame at the `monitorenter` (or the
  `synchronized` method's entry), so both succeed. HotSpot answers `OPAQUE_FRAME` only for a
  native top frame and for `Object.wait`.
- CratonVM: the thread waits in a blocking region, not at a suspend point.
  `debug::run_on_parked_thread` answers `NotParked`, and `native_env::frame_op_on_parked` maps
  that to `OPAQUE_FRAME` (32) for a suspended thread.

The JDWP side (`ThreadReference.PopFrames` / `ForceEarlyReturn`) goes through the same parked-thread
runner and is expected to answer the same way; check it when the probe row exists.

## What a fix needs

1. A probe row first: `tools/probes/interp/L1/L1W61JvmtiFrameControl.java` (or a new
   `L1W1xxJvmtiFrameControlBlockedOnMonitor`) with a thread blocked on a held monitor, suspended,
   then `PopFrame` and `ForceEarlyReturnInt`, MEASURED on HotSpot 25.0.3 with the MSVC-built
   shim, default flags and `-Xint`. This settles whether HotSpot really serves both (the reading
   above is not a measurement).
2. Record the request through the blocked thread's inspection window, as `NotifyFramePop` of a
   blocked thread is recorded (wave 61).
3. Apply it when the thread leaves the blocking region, before it takes the monitor: a forced
   return, or a pop of a frame that never entered the monitor. The monitor-entry path must look
   at the record; wave 54's `ExitOnSuspend` path is the place.
4. Behind a parity switch (default ON with `=0` once the probe matches HotSpot in both modes).

## Related, not covered here

- A thread suspended in a compiled loop before an exit-capable poll (i61 item 3): see
  `docs/internal/proposals/interpreter/i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands-20261007.md`.
  Its wave-124 section explains why a suspension does not leave a thread compiled here
  (`polling_body_must_leave`, `compiled_loop_must_leave`), so this VM parks it interpreted
  and serves the request.
- An unmounted virtual thread (i61 item 3, second half): HotSpot reads the continuation. Not
  examined here.

## Wave 134 (lane L1), 2026-12-31

**Status: OPEN (code DONE, not built, not measured).** The fix is in, behind the parity switch
`CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL` (`VmConfig::jvmti_blocked_entry_frame_control`,
default ON, `=0` answers `OPAQUE_FRAME` again). It needs
`CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` (default ON), whose loop reader takes the request.
The page closes once the probe below matches a MEASURED HotSpot block in both modes.

What changed (items 2 and 3 of "What a fix needs"):

- `jvmti::native_env::frame_op_on_parked`: when the parked route answers `NotParked` for a
  suspended thread, the request goes through the thread's inspection window
  (`with_blocked_frames`). `blocked_entry_frame` serves only the following: block state 2
  (BLOCKED), a mounted thread, and a listing top row that is the top interpreter frame, run
  interpreted, standing at a `monitorenter` (`last_instr_pc`) or at bytecode 0 of a
  `synchronized` method whose frame was pushed before its contended monitor
  (`CRATONVM_SYNC_METHOD_FRAME_FIRST`, default on). Everything else keeps `OPAQUE_FRAME`.
  `PopFrame` also checks the caller row, which must be an interpreter frame (otherwise
  `NO_MORE_FRAMES` / `OPAQUE_FRAME`), and runs the parked pop's frame checks on the window's
  frames (`debug::pop_frames::checked_blocked_pop`).
- The request is recorded in `DebugState::deferred_frame_pops[tid].control`
  (`debug::early_return::defer_blocked_pop` / `defer_blocked_forced_return`). The refusals
  follow HotSpot's order: a second request `INTERNAL`, the frame `OPAQUE_FRAME`, the value
  `TYPE_MISMATCH`. A `ForceEarlyReturnObject` value is held by a JNI global reference.
- At the wake (`inspect::apply_deferred_frame_writes`, which already ran wave 61's
  `NotifyFramePop`s), `early_return::adopt_blocked_frame_control` checks that the top frame is
  still the one named. It then turns the request into the records a callback point leaves
  (`jvmti_pops` / `jvmti_pending`, the forced value at `depth - 1` when it was made after the
  pop), raises `AGENT_REQUEST_FRAME_CONTROL` and bumps the loop poll word. The thread now holds
  the monitor, and the dispatch loop's next top takes the request
  (`take_callback_frame_control`). The pop or the forced return releases the monitor with the
  frame, as HotSpot's `check_and_handle_popframe` / `check_and_handle_earlyret` do after
  `InterpreterRuntime::monitorenter` returns (by reading). A synchronized entry that adopted a
  request posts no `MethodEntry` for the frame
  (`vm_exec::enter_synchronized_method_in_pushed_frame_with`), because HotSpot takes it in
  `lock_method` before `notify_method_entry` (by reading).
- Item 3 differs from the page's wording: the request is taken after the monitor is held,
  not before. That is HotSpot's order by reading. The thread waits for the owner either way.

Residuals (not fixed):

- No loop is known when the request is checked. If the popped frame is the entry frame of a
  nested dispatch loop (its caller runs in another loop), the `PopFrame` already answered
  `NONE`. The take then drops it (`pop_frames::drop_jvmti_pop_at`, called from
  `interpreter::deliver_breakpoint_if_set`; `CRATONVM_FRAME_TRACE=1` prints
  `[POP_FRAMES] jvmti dropped at a loop's entry frame`). HotSpot answers `OPAQUE_FRAME` when
  the caller is a call stub, so this matters only where CratonVM nests a loop that HotSpot
  does not.
- A compiled or `runs_compiled` top frame, and `CRATONVM_SYNC_METHOD_FRAME_FIRST=0` (the
  caller is on top), keep `OPAQUE_FRAME`.
- JDWP `ThreadReference.PopFrames` / `ForceEarlyReturn` of such a thread are unchanged.

Probe (new): `tools/probes/interp/L1/L1W134JvmtiBlockedEntryFrameControl.java` + `.c`, rows
`blocked int`, `blocked pop`, `blocked long`, `blocked pop then int`, `blocked sync int`,
`blocked sync pop` and the control `waiting`. Its HOTSPOT_EXPECTED block is written from the
JVMTI spec and from reading HotSpot, NOT MEASURED (this lane built no shim).

Orchestrator, in order:

1. `cargo test -j 5 -p cratonvm-vm --features experimental-debug --lib -- i134_l1`. The new
   tests are `debug::early_return::i134_l1_blocked_frame_control_tests` (4),
   `jvmti::native_env::i134_l1_blocked_entry_frame_control_tests` (3) and
   `runtime::interpreter::jvmti_events::i134_l1_contended_entry_tests` (2). Also run the
   existing `debug::pop_frames` tests, `i95_l7_agent_request_word_tests`,
   `gci_i11ja_poster_thread_tests`, and the types flag-surface test
   (`cargo test -j 5 -p cratonvm-types`). Pass: all green.
2. Build the shim with MSVC and record HotSpot 25.0.3 (default flags and `-Xint`) with the
   commands in the probe header. Write the measured block over the reading block. If they
   differ, the CratonVM target is the MEASURED block.
3. Run the debug `cratonvm` (`experimental-debug`) in `--jdk-only` and `--compatible`, JIT
   and `--nojit`. Pass: the six `blocked *` rows and `waiting` equal HotSpot's measured
   lines. Negative control: `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL=0` gives 32 for
   every `blocked` action, with the calls running on (`calls=1 inside=1 returned 107`;
   `calls=1 inside=0 returned 109` for the sync rows). Positive control:
   `CRATONVM_FRAME_TRACE=1` prints `[POP_FRAMES] jvmti deferred` /
   `[JVMTI_EARLY_RETURN] deferred`, then `[BLOCKED_FRAME_CONTROL] adopted`.
4. Regressions: `L1W61JvmtiFrameControl` and `L1W62JvmtiCallbackFrameControl` unchanged.
5. If a row differs from HotSpot's MEASURED line and cannot be fixed, flip the default to OFF
   (`runtime_flag_on`), keep the switch, and leave this page OPEN with the measured lines.
