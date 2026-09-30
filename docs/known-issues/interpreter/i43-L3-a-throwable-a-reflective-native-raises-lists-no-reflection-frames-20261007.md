# Where a reflective call's JDK frames are still not listed

**Status: open (item 4 only, lane L1's file; item 2 fixed in wave 44, item 3 and item 1's `InvocationTargetException` in wave 45, item 1's argument checks in wave 46, item 5 closed in wave 46 as not runnable) — filed 2026-10-07 by interpreter round i1 wave 43, lane L3
(the remainder of
`docs/internal/fixed-bugs/interpreter-L3-a-reflective-call-leaves-no-method-invoke-frame-FIXED-20261007.md`).
Both modes.**

Since wave 43 a capture made inside the target of `Method.invoke` /
`Constructor.newInstance` lists HotSpot's JDK frames for the call
(`stackwalker::reflective_splices`). It lists nothing for a call in these
cases, each deliberate (fail-closed) or out of the lane's files:

1. **A throwable the reflective native raises itself.** The capture lists a
   call's frames only below an interpreter frame the call pushed; an
   `InvocationTargetException` (built after the target returned), an
   argument check's `IllegalArgumentException` or a receiver check's
   `NullPointerException` has none above the call. HotSpot's trace of such a
   throwable starts in the accessor, at another line than the frames listed
   for a target (measured, `L3W43ReflectionFrames` row
   `invocation-target-exception-top`:
   `jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.lambda$main$11`;
   CratonVM, read from the code: the caller first). Fix: the native names the
   JDK frame it stands at when it raises (DMHA `invoke` at its `throw new
   InvocationTargetException` line, `checkArgumentCount` for the count check,
   and so on), listed on top of the capture as the round-13 stand-in frames
   are (`stackwalker::native_standin_frames`).
2. **A compiled activation entered at the call's interpreter depth after the
   call** -- the target, or a callee of it, run compiled without an
   interpreter frame. Its chain entry has the call's depth, so the capture
   cannot tell it from the call's compiled caller, and lists nothing for the
   call (`conservative_roots::jit_chain_entered_at_depth_since`). A trace
   through a reflective call can therefore differ between `--nojit` and the
   JIT. Fix:
   `docs/internal/fixed-bugs/interpreter-L3-proposal-compiled-activations-name-their-jit-entry-FIXED-20261008.md`.
3. **Other threads.** `Thread.getStackTrace()` of another thread, thread
   dumps (`getAllStackTraces`) and the published blocking snapshots are built
   from `thread_registry`'s frame traces, which know nothing of
   `JvmThread::reflective_calls`. HotSpot lists the frames there too.
4. **JVMTI `GetStackTrace` / JDWP `ThreadReference.Frames`**
   (`jvmti::native_env::frames_of`, lane L1's files): HotSpot lists the two
   frames as Java frames of the thread; CratonVM does not (it lists a JNI
   native's row since wave 42, the same kind of splice).
5. **JDK 17 / 21 reflection.** The frames are named from JDK 25's
   `DirectMethodHandleAccessor` / `DirectConstructorHandleAccessor` bytes; on a
   JDK whose reflection runs through `NativeMethodAccessorImpl` (JDK 17) the
   steps are not found and nothing is listed.

## Evidence

Items 1, 3, 4: read from the code (`stackwalker::reflective_splice_positions`
refuses a call with no interpreter frame above it;
`thread_registry::frame_trace_of_resolved` and `native_env::frames_of` take no
reflective record). Item 2: the refusal is the positive control's
`compiled_since=true` under `CRATONVM_DBG_STTRACE=1`. Item 5: the step tables
`METHOD_INVOKE_STEPS` / `CONSTRUCTOR_NEW_INSTANCE_STEPS`.

## Progress (wave 44) — lane L3

* **Item 2 fixed.** Compiled activations name the JIT entry-chain entry they
  came from (`ActiveCompiledFrame::chain_index`), and a call's frames go
  right before the first slot pushed after it
  (`stackwalker::trace_anchor_position`): interpreter frame `d`, or a
  compiled activation entered at depth `d` or deeper after the call. The
  capture no longer refuses a call whose target runs compiled.
  `jit_chain_entered_at_depth_since` is gone. Probe
  `tools/probes/interp/L3/L3W44ReflectionCompiledTarget.java` (a compiled
  caller, a compiled target: `Throwable`, `Thread.getStackTrace`, the two
  walks and a throw through the call); `CRATONVM_DBG_STTRACE=1` prints
  `compiled_after=true` where the frames went below a compiled target. The
  proposal is closed:
  `docs/internal/fixed-bugs/interpreter-L3-proposal-compiled-activations-name-their-jit-entry-FIXED-20261008.md`.
* **Item 4's placement is ready, the listing is not.** JVMTI's stack
  (`native_env::frames_of`) places its native rows with the same helper now
  (`stackwalker::capture_trace_with_anchor_positions`); listing a
  reflective call's two frames there would take the thread's
  `reflective_calls` as more anchors and name the frames' methods by their
  class and index (`reflective_entries` builds entries with both). Lane L1's
  file.
* **Items 1, 3 and 5 untouched.** Item 1 needs a per-exception table of
  the JDK frame each native raise stands at (HotSpot's `DMHA.invoke` at its
  `InvocationTargetException` line for a target's throw, other frames for
  the argument checks), measured per row before it is built. Item 3 needs
  the published trace (`capture_published_trace`, run by the thread itself)
  to splice the calls, with its `frame_positions` shifted to match, and must
  not take the class-manager lock at a blocking deposit (it may be held):
  only the memoized names (`JvmThread::reflective_frames_named`) may be used
  there.

## Progress (wave 45) — lane L3

* **Item 1, the `InvocationTargetException`: fixed.** The native builds it
  inside the call's record by running its real constructor
  (`lang_class::wrap_as_invocation_target_exception`), so the call's first
  pushed frame is `InvocationTargetException.<init>`, the first frame the
  fill-frame trim removes. `stackwalker::build_slots_with_splices` now lists
  a call at exactly that position (or one that pushed nothing,
  `reflective_splice_positions(.., unanchored_on_top = true)`, throwable
  captures only) ON TOP of the trace; `vm_exec::attach_reflective_raise`
  gives it the accessor's `throw new InvocationTargetException(e)` line
  (`stackwalker::reflective_raise_entries`: the first, second or last `new`
  of the throwable's class in the innermost JDK frame's method, picked by the
  class of what the constructor was handed -- `ClassCastException` /
  `WrongMethodTypeException`, `NullPointerException`, anything else -- the
  accessor's three `catch` arms). A `Constructor.newInstance` of a throwable
  class, whose own constructor fills the trace, lists the frames at their
  call lines, as HotSpot does. Probe
  `tools/probes/interp/L3/L3W45ReflectiveRaise.java` (rows `ite-*`,
  `ctor-of-a-throwable`); `L3W43ReflectionFrames`'s
  `invocation-target-exception-top` row now prints HotSpot's line. Control:
  `CRATONVM_DBG_STTRACE=1` prints `STTRACE_DBG_REFLECT raise
  throwable=java/lang/reflect/InvocationTargetException cause=Other
  relined=true` and `STTRACE_DBG_REFLECT on-top listed=2 raise=true`.
  Unit test `a_throwable_raised_by_a_reflective_native_lists_the_call_on_top`.
  Under `--compatible`, where `InvocationTargetException` may be a synthetic
  stub built through `ReflectiveOperationException.<init>()V`, the screen
  fails and the frames are listed at their call lines (names as HotSpot, the
  accessor's line 104 instead of 119).
* **Item 1, the argument checks: open.** They are `RuntimeError`s
  (`lang_class::illegal_arg_exc`) the interpreter materializes AFTER the
  native returned and left the record, so nothing is listed. HotSpot, JDK
  25.0.3 (`L3W45ReflectiveRaise`):
  * argument count: `DirectMethodHandleAccessor.checkArgumentCount:324`,
    `DirectMethodHandleAccessor.invoke:102`, `Method.invoke:565`;
  * argument type: `DirectMethodHandleAccessor.invoke:108`, `Method.invoke:565`;
  * receiver type: `DirectMethodHandleAccessor.checkReceiver:199`,
    `DirectMethodHandleAccessor.invoke:100`, `Method.invoke:565`;
  * null receiver: `Method.invoke:557` alone (the NPE is implicit in
    `Method.invoke`, before the accessor);
  * null to a primitive: the `IllegalArgumentException` is built inside the
    record (`illegal_arg_exc_null_to_primitive`) and, read from the code,
    now prints HotSpot's
    `DirectMethodHandleAccessor.invoke:114` (its cause is a
    `NullPointerException`); its cause's own trace starts at
    `sun.invoke.util.ValueConversions.unboxInteger:81` on HotSpot, which the
    native does not run (CratonVM: the accessor at its call line, 104).
  Next step: in `lang_reflect::native_method_invoke_boxed` (and the
  constructor's twin), materialize a `RuntimeError` the body returns while the
  record is still on, with a per-check hint on the thread (which check
  failed) that `reflective_raise_entries` turns into the rows above: an extra
  JDK frame on top for `checkArgumentCount` / `checkReceiver` (at their `new
  IllegalArgumentException`), `invoke` at the call of that check; for the
  null receiver, `Method.invoke` alone at its line.
* **Item 3: fixed** (the proposal, built:
  `docs/internal/fixed-bugs/interpreter-L3-proposal-published-traces-carry-the-reflective-calls-FIXED-20261009.md`;
  probe `tools/probes/interp/L3/L3W45PublishedReflectiveTrace.java`). HotSpot's
  `getAllStackTraces` / `ThreadInfo` also list the hidden method-handle
  frames between the accessor and the target (`Thread.getStackTrace` does
  not); CratonVM runs none and lists none.
* **Items 4 and 5: untouched.** Item 4 is lane L1's file
  (`jvmti::native_env::frames_of`), which wave 45's L1 lane is rewriting for
  other threads' stacks; the anchors are ready
  (`capture_trace_with_anchor_positions`), and the frames can come from the
  thread's memo as the published trace's do (`reflective_splices_memoized`,
  now filled at call entry). Item 5 needs a JDK 17 to read the
  `NativeMethodAccessorImpl` / `DelegatingMethodAccessorImpl` bytes and its
  inflated `GeneratedMethodAccessor<n>` frames; none is installed on the
  lane's machine.

## Progress (wave 46) — lane L3

* **Item 1, the argument checks: fixed.** Each check of
  `lang_class::native_method_invoke` and
  `native_constructor_new_instance_body` now raises through
  `lang_class::raise_reflective_check`: it notes which check failed on the
  call's record (`NativeExceptionAccess::note_reflective_check`, a new
  `ReflectiveCheck` on `ReflectiveCallRow::check`) and builds the check's
  throwable by its real constructor INSIDE the call, so the capture lists the
  call on top as it does an `InvocationTargetException`'s
  (`build_slots_with_splices`, wave 45). `vm_exec::attach_reflective_raise`
  asks `stackwalker::reflective_check_entries` for the check's frames first:
  * argument count: `DirectMethodHandleAccessor.checkArgumentCount` at its
    `new IllegalArgumentException`'s constructor call over `invoke` at its
    call of the check (`:324`, `:102`); the constructor accessor's first
    `new` (`newInstance:59`);
  * receiver type: `checkReceiver:199` over `invoke:100`; a null receiver:
    `Method.invoke:557` alone (its access check's `obj.getClass()`), or
    `checkReceiver:197` over `invoke:100` for a `setAccessible(true)` method
    (the native reads the `override` flag);
  * argument type / null to a primitive: the accessor's first / second
    `new IllegalArgumentException` (`invoke:108` / `:114`); the constructor
    accessor's second / third (`newInstance:65` / `:70`; wave 45 put a
    constructor's null-to-primitive at `:65`, the type arm);
  * the null-to-primitive `IllegalArgumentException`'s CAUSE (the
    `NullPointerException`): the call's frames at their call lines with
    `sun.invoke.util.ValueConversions.unbox<Wrapper>` on top at its
    `primitiveConversion(...).<x>Value()` call (`unboxInteger:81`,
    `unboxLong:126`, `unboxBoolean:108`, ...); the note loads
    `ValueConversions` so the capture can name it.
  A frame is placed at the pc of the constructor CALL that completes the
  `new`, not the `new` (`stackwalker::throwable_init_sites`): HotSpot's
  frame stands there, and `checkReceiver`'s `new` (line 198) and its
  constructor call (line 199) are on different lines. Probe
  `tools/probes/interp/L3/L3W46ReflectiveChecks.java` (24 rows, HotSpot 25.0.3
  measured, the same with `-Xint`); `L3W45ReflectiveRaise`'s four
  argument-check rows and its `iae-null-to-primitive-cause` row now print
  HotSpot's lines too. Control: `CRATONVM_DBG_STTRACE=1` prints
  `STTRACE_DBG_REFLECT raise throwable=java/lang/IllegalArgumentException
  cause=Other relined=true check=Some(ArgumentCount)` for the probe's
  `m-arg-count` row (the base prints no `raise` line for it). Unit tests
  `a_reflective_check_stands_at_its_arms_constructor_call`,
  `nested_throwable_sites_pair_with_their_own_constructor_calls`. Both modes
  (a genuine bug in both; the `RuntimeError` the interpreter built after the
  native returned becomes the same class and message built inside it).
  Two divergences found while measuring: the null-to-primitive
  `IllegalArgumentException`'s message (fixed with it: the probe's
  `*-null-to-primitive-kind` rows) and the constructor type mismatch's
  missing `ClassCastException` cause (open), filed as
  `docs/known-issues/interpreter/i46-L3-a-reflective-argument-checks-message-and-cause-differ-from-hotspots-20261010.md`.
* **Item 5: closed as not runnable.** JDK 17's reflection
  (`NativeMethodAccessorImpl` / `DelegatingMethodAccessorImpl`, inflated
  `GeneratedMethodAccessor<n>`) needs a JDK 17 run to measure HotSpot's
  frames and lines, and neither the lane's machine (only JDK 25.0.3) nor the
  round's record of the host names a JDK 17 install. The behaviour on such
  an image is fail-closed, as the section comment in `stackwalker.rs` says:
  the steps are not found and nothing is listed for the call (the trace of
  every release before wave 43). JDK 18+ reflection is the method-handle
  accessor this page's steps name (JEP 416); the lines are read from the
  running image's own `LineNumberTable`s, not hard-coded. Reopen with a JDK
  17 image and the frames it prints.
* **Item 4: untouched** (lane L1's `jvmti::native_env::frames_of`; wave 46's
  L1 lane has it as its item 4, coordinated through this section). The check
  records added here need nothing from it: JVMTI lists the thread's frames,
  not a throwable's.

## What remains

Item 4's remainder only: JDWP `ThreadReference.Frames` and a thread blocked
in a native region (see "Progress (wave 46) — lane L1" below; tracked on
`i46-L1-a-thread-blocked-in-a-native-lacks-two-c-jvmti-answers-20261010.md`
and the one-listing proposal). When it lands, move this page to
`docs/internal/fixed-bugs/interpreter-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-FIXED-<date>.md`
and fix the citations (`vm/src/runtime/stackwalker.rs`,
`native-builtins/src/lang_class.rs`, the L3 probes).

## Progress (wave 46) — lane L1

* **Item 4, the C JVMTI table: fixed for the current thread and a thread
  parked at a suspend point.** `jvmti::native_env::listed_rows` anchors
  each of the thread's `reflective_calls` with
  `stackwalker::capture_trace_with_anchor_positions` and splices the frames
  `stackwalker::reflective_entries` names right above the first frame the
  call pushed, as the capture does for a stack trace. HotSpot 25.0.3's
  `GetStackTrace` (measured, `tools/probes/interp/L1/L1W46JvmtiReflectiveFrames.java`)
  lists `DirectMethodHandleAccessor.invoke@23`, `Method.invoke@102` for
  `Method.invoke`, and `DirectConstructorHandleAccessor.newInstance@60`,
  `Constructor.newInstanceWithCaller@41`, `Constructor.newInstance@30` for
  `Constructor.newInstance`, which the listing now gives; it also lists the
  accessors' `@Hidden` `invokeImpl` and the method-handle frames under it,
  which this VM does not run (the probe leaves them out). A thread parked
  at a suspend point lists itself the same way (wave 46). Positive control:
  `CRATONVM_FRAME_TRACE=1` prints `[JVMTI_NATIVE_ROWS] natives=1
  reflective=1 ...`.
* **Item 4, what remains:** JDWP's `ThreadReference.Frames` (the listing a
  park publishes, `interpreter::publish_frame_snapshot`) and the C table's
  listing of a thread blocked in a native region
  (`interpreter::read_blocked_rows`) still list no reflective frame: filed
  with the blocked case as
  `docs/known-issues/interpreter/i46-L1-a-thread-blocked-in-a-native-lacks-two-c-jvmti-answers-20261010.md`
  (item 2), and the one-listing fix as
  `docs/known-issues/interpreter/i46-L1-proposal-one-frame-listing-for-jdwp-and-the-c-jvmti-table-20261010.md`.
  Item 4 can close with that page (the JDWP half is the proposal's step 3).

