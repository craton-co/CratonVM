# JIT round 14 proposals, lane trace (wave 3)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: stack-trace capture of VM-served JDK frames, `StackTraceElement` materialisation
Found by: round 14 wave 3 lane trace

Ranked by expected benefit over cost. Wave 2's book is `jit-r14-trace-proposals.md` (T14-1..5).

## T14W3-1. A marker frame for natives that call back into Java

**What.** Every VM-served JDK method that calls user code (`Thread.run`'s Bridge, the remaining
`--compatible` collection / functional natives, `AccessController.doPrivileged`-style shims,
reflective `Method.invoke` fast paths) loses its own frame from every trace, and each gap has
been closed so far with a hand-written stand-in (`native_standin_frames`, the argument-check
kind, wave 3's `thread_run_standin_frame`). Instead, let the native-call funnel push a
zero-cost marker record (class id, method index, the bci of the JDK body's call site, resolved
once per method from the real class bytes) on the thread's frame stack while a native that is
KNOWN to call back runs; the walk turns a marker into an ordinary frame. **Benefit:** middle
frames (`Sub.run` -> `super.run()` -> task; `Optional.map` under the synthetic JDK) and
`StackWalker` / thread dumps come for free, and the stand-in census stops growing. **Cost:**
medium (the funnel, one flag per registration, the walk). **Risk:** a marker left behind on an
unwinding path -- pop it in the funnel's drop guard. **First step:** census which registered
natives call `invoke_virtual` / `invoke_static` (grep `native-builtins` for `ctx.invoke_`) and
how many of them sit over real bytecode under `--jdk-only`.

## T14W3-2. `Thread.run` below `StackWalker` and other threads' stacks

**What.** Wave 3's `Thread.run` stand-in applies to throwable captures only. `StackWalker.walk`
(`NativeExceptionAccess::capture_stack_walk_trace`) and `Thread.getStackTrace()` of another
thread (`threading::thread_registry`) still end at the task's `run`. Call
`prepend_thread_run_standin_frame` there too (no depth cap for the walk; the thread's own
`java_thread_obj`). **Benefit:** `StackWalker.getCallerClass`-free walkers (logging frameworks,
`Thread.getAllStackTraces()` dumps) see HotSpot's bottom frame. **Cost:** small (two call sites
in `vm_exec.rs` / `thread_registry.rs`). **Risk:** a `StackWalker` consumer that counts frames
from the bottom -- none known. **First step:** a probe printing
`StackWalker.getInstance().walk(s -> s.reduce((a, b) -> b))` inside a started thread.

## Round 14 wave 3 (lane trace2): T14W3-2 landed

`StackWalker` half landed: `NativeExceptionAccess::capture_stack_walk_trace` (`vm_exec.rs`) now calls
`add_thread_run_walk_entries` (bottom frame through the shared `thread_run_bottom_frame`, plus the
T14W3-3 middle frames) for a platform thread; no depth cap. Switch
`CRATONVM_STACK_WALK_THREAD_RUN_FRAMES` (default on, both modes). `Thread.getStackTrace()` of ANOTHER
thread and dumps (`thread_stack_trace`, outside the lane's regions) are an exact patch page:
`r14w3-trace2-thread-stack-trace-thread-run-frame-patch-FIXED-20260929.md`. Not done: the `@Hidden`
`Thread.runWith` frame a `SHOW_HIDDEN_FRAMES` walker sees on HotSpot between the task and
`Thread.run`. Probe `C:\craton\jitr14-probes\src\R14Trace2ThreadRunWalk.java` (`walk-*` rows).

## T14W3-3. The middle `Thread.run` frame (`super.run()` in a Thread subclass)

**What.** `class W extends Thread { public void run() { ...; super.run(); } }` (JBoss
`JBossThread`, Netty `FastThreadLocalThread` wrappers) shows `task.run | W.run` where HotSpot
shows `task.run | Thread.run | W.run`. Insert the frame when frame i stands at an invoke that
resolves to `java/lang/Thread.run()V` (interpreted: the bci's `invokespecial` / `invokevirtual`
owner and name; compiled: the label bci's invoke decoded the same way) and frame i+1 is a
`run()V` of the task class the thread's `holder.task` names. **Benefit:** the common
framework-thread shape. **Cost:** small-medium. **Risk:** an `invokevirtual t.run()` on a
DIFFERENT thread object (`otherThread.run()` called directly) -- screen by requiring the
receiver's task class, which a direct call on another thread fails only when both tasks share a
class; accept that, or read the receiver from the interpreter frame's operand stack.
**First step:** `R14TraceThreadRun` `superRun` row.

## Round 14 wave 3 (lane trace2): T14W3-3 landed

`stackwalker::thread_run_middle_frame` (+ lock-free `may_need_thread_run_middle_frame`), applied by
`vm_exec.rs` `insert_thread_run_middle_frames` (throwable capture, BEFORE the `--jdk-only` hidden drop,
re-capped to `MaxJavaStackTraceDepth`, also in the redo-whole path) and `add_thread_run_walk_entries`
(`StackWalker`). The screen is not the task class proposed above but the call itself: the caller
stands at an `invokespecial` / `invokevirtual` whose `run()V` resolves (first declaration up the
owner's superclass chain) to `java.lang.Thread`'s own, and the callee is a `run()V` of a non-`Thread`
class; an `invokevirtual` whose callee's class extends `Thread` is refused (that was the receiver's
override). So a direct `new Thread(r).run()` gets the frame too, as on HotSpot, and no thread-object
read is needed. Switch `CRATONVM_THROWABLE_THREAD_RUN_MIDDLE_FRAME` (default on, both modes). Unit
tests `r14w3_trace2_thread_run_middle_tests`. Probe rows `*-super`, `*-direct`, `*-hotdirect` of
`R14Trace2ThreadRunWalk`, and `superRun` of `R14TraceThreadRun`.

## T14W3-4. A per-VM origin record per class

**What.** Wave 3's T3-3 memo lives for one array build. A per-VM side record per `ClassId`
(module name, version and loader-name strings as Rust `Arc<str>`, the `format` bits) would also
serve `StackTraceElement.of` paths that build one element at a time
(`native_throwable_get_stack_trace_element`) and the native printer
(`throwable_frame_text`). **Benefit:** printing a trace stops taking the class-manager lock per
frame. **Cost:** a `NativeContext` accessor over a `ClassManager` side table (redefinition-stable;
dropped on class unload). **Risk:** none beyond the unload hook. **First step:** time
`printStackTrace` of a 150-frame trace with `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO=0` vs `1`
to see how much the per-build memo already recovered.
