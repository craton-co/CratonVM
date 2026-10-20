# i133-L4: a `--nojit` recursion through `invokeWithArguments` overflows the native stack

Interpreter round i6, wave 133, lane L4. Found by the orchestrator on the w131 build (already
present on w124). Status: FIX WRITTEN, NOT BUILT (`CRATONVM_EXEC_NATIVE_STACK_GUARD`, parity,
default ON, `=0` off). Move to `docs/internal/` once the commands below pass.

## Symptom

`tools/probes/interp/L4/L4W130ExactPushAndLinkMemo.java`, row `B soe`, under `--nojit`, both
modes, default switches: `rec(x)` calls `REC.invokeWithArguments(x)` (where `REC` is `rec`
itself) until `StackOverflowError`. HotSpot 25.0.3 (default and `-Xint`) prints
`B soe: java.lang.StackOverflowError`. CratonVM died with a native `EXCEPTION_STACK_OVERFLOW`
(exit 127): no Java exception, no output after the row. With the JIT the row was SAME.

## Cause

Each level of the recursion is a nested interpreter entry made from Rust: the
`invokeWithArguments` door (`native-builtins/src/lang_invoke.rs`) runs the target through
`interpreter::execute` or, on the doors' fast path, `install_and_run_cached_frame` ->
`run_pushed_frame_to_completion`. Three bounds existed, none of which looks at the stack pointer:

- `max_stack_depth` (8192 Java frames): a level holds few Java frames, so it is far away.
- `EXEC_DEPTH` / `EXEC_DEPTH_CEILING` in `execute`: counts levels against an estimate of 8 KiB of
  native stack per level, over half the stack. The served `invokeWithArguments` levels cost more
  than 16 KiB each, so the stack ran out first.
- `run_pushed_frame_to_completion` counted nothing.

The JIT has checked the real stack pointer since round 13 (`jit_native_stack_exhausted` in
`enter_jit_dispatch`), which is why the JIT arm was SAME.

## Fix (wave 133, lane L4; not built)

`vm/src/runtime/interpreter.rs`: `reentry_native_stack_exhausted()` compares the stack pointer
with a per-thread floor (`low + reentry_stack_margin(size)`, 512 KiB on the 8 MiB and 128 MiB
carriers, never more than half a small stack; the bounds come from
`jit::helpers::thread_stack_bounds`, queried once per thread). `execute` (before the
`EXEC_DEPTH` count) and `run_pushed_frame_to_completion` (before running the pushed frame, which
is then popped without consulting its handlers, as HotSpot raises at the callee's entry) ask it.
A trip builds the `StackOverflowError` in place through `throw_runtime_error`, with the guard
suspended while the constructor runs Java below the floor (as `raise_jit_stack_overflow` does for
the JIT floor). A stack pointer outside `[low, floor]` never trips. The switch is read only below
the floor. The margin sits below the JIT's compiled-recursion floor (up to 1 MiB above the low
end), so with the JIT on the compiled guard keeps tripping first.

New probe: `tools/probes/interp/L4/L4W133NativeReentrySoe.java` (recursion through
`invokeWithArguments`, `invoke`, `invokeExact`, `Method.invoke`, `Constructor.newInstance`, and
`invokeWithArguments` on a second thread), each followed by a recovery call.

## Pass criteria

1. `L4W130ExactPushAndLinkMemo`: `--jdk-only` and `--compatible`, `--nojit` and JIT, every arm
   its header lists: output equals the `HOTSPOT_EXPECTED` block (`B soe:
   java.lang.StackOverflowError`, `B after-soe: 14 14`), exit 0.
2. `L4W133NativeReentrySoe`: same four runs, output equals its block, exit 0.
3. `CRATONVM_EXEC_NATIVE_STACK_GUARD=0 --nojit` on `L4W130ExactPushAndLinkMemo`: the old crash
   (exit 127) returns, which shows the guard is what fixed it.
4. `cargo test -p cratonvm-vm i133_l4_reentry_stack_guard_tests` and the flag tests
   (`cargo test -p cratonvm-types`).

## What would make the guard unnecessary

Nothing in the interpreter: a nested entry from Rust always uses native stack, and only the real
stack pointer bounds it. The count-only `EXEC_DEPTH` ceiling stays as the second bound.

## Wave 134 (lane L4), 2026-10-19

Status: FIX WRITTEN, NOT BUILT (review of w133, doubtful items 3 and 4). Still OPEN until the
wave-133 criteria above and the runs below pass.

1. `execute`'s trip returns the lazy `InternalError(Runtime(StackOverflowError))`, as the
   `EXEC_DEPTH` trip next to it does; the caller's dispatch loop converts it with the guard
   armed, so no Java runs below the floor and the margin is not relied on. Nothing is pushed and
   no monitor is held at that point (the synchronized entry comes later in `execute`). New switch
   `CRATONVM_EXEC_NATIVE_STACK_GUARD_LAZY_ERROR` (parity, default ON, `=0` builds the error in
   place as in wave 133; read only on a trip).
2. `run_pushed_frame_to_completion`'s trip:
   - applies only to one pushed frame that has not run (bytecode index 0, no instruction
     executed; `reentry_trip_applies`). A deopt resume's frames already ran compiled, and popping
     them skipped their handlers, so they now run on and the next fresh nested entry trips;
   - (a) checked, no defect: no frame that reaches it is a `SyncEntry::Deferred` entry. The doors
     acquire uncontended only (`door_monitor_acquire`), the compiled doors decline a contended
     entry under `CRATONVM_SYNC_METHOD_FRAME_FIRST` (`frame_first_door_try`), and the only
     deferred push is `execute`'s own, whose guard runs before it. A held `monitor_on_exit` is
     released by the pop, and an unheld one is never exited;
   - (b) a deferred `MethodEntry` record naming the pushed frame is dropped, as the frame-limit
     trip in `invoke.rs` drops it. The frames are popped as an exception unwind
     (`pop_unrun_frames_by_exception_keeping_result`: `MethodExit` and `FramePop` with
     `was_popped_by_exception`), which pairs the `MethodEntry` the push already posted, and a
     `NotifyFramePop` requested from that `MethodEntry` is consumed by the pop. Before, the pop
     posted `FramePop(false)` and no `MethodExit`;
   - the error is still built in place there, because its callers include the JIT's one-shot
     resumes, for which an internal error is a re-run request rather than an exception.

Runs (orchestrator):

- `tools/probes/interp/L4/L4W130ExactPushAndLinkMemo.java`, row `B soe` and `B after-soe`:
  `cratonvm --jdk-only --nojit` and `cratonvm --compatible --nojit` (every arm the header lists),
  output equals HotSpot `java -Xint` (its `HOTSPOT_EXPECTED` block), exit 0. Also once with
  `CRATONVM_EXEC_NATIVE_STACK_GUARD_LAZY_ERROR=0` (the wave-133 build in place; same pass).
- `tools/probes/interp/L4/L4W133NativeReentrySoe.java`: same runs, both modes, JIT and `--nojit`,
  output equals its block, exit 0.
- `cargo test -p cratonvm-vm i133_l4_reentry_stack_guard_tests` (three new tests: the lazy arm
  first in `execute`, the pushed-frame trip's cleanup, the switch default) and
  `cargo test -p cratonvm-types`.
- Not run, would close item 3 for agents: a synchronized contended callee recursing through
  `invokeWithArguments` under a C JVMTI `MethodEntry`/`MethodExit` agent, counting entries and
  exits (they must be equal).
