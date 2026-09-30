# JIT round 14 proposals, lane trace2 (wave 3)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: VM-served JDK frames in stack captures (throwable, `StackWalker`, other threads' stacks)
Found by: round 14 wave 3 lane trace2

Ranked by expected benefit over cost. The previous books are `jit-r14-trace-proposals.md` and
`jit-r14-trace3-proposals.md`.

## T4-1. One served-frame pass for every capture kind

**What.** The served-frame rules (the sleep / wait leaf census, the argument-check kind, the bottom and
middle `Thread.run` frames) are now applied at three call sites with three frame types: the throwable
capture (`BacktraceFrame`, `vm_exec.rs`), the `StackWalker` walk (`StackTraceEntry`, wrapped into
`BacktraceFrame::Entry` clones pair by pair) and, once its patch page lands, `thread_stack_trace`.
Give `stackwalker.rs` a small trait (`class id`, `method name / descriptor`, `call bci`) implemented by
both frame types and one `apply_served_frames(store, thread, trace, kind)` that owns the order (middle
before the hidden drop, leaf, bottom, re-cap). **Benefit:** no per-pair clones in walks; one place to
add the next rule; the orders cannot drift between capture kinds. **Cost:** small-medium (mechanical).
**Risk:** low; the unit tests of each rule stay. **First step:** move the three screens behind the
trait and make `add_thread_run_walk_entries` clone-free.

## T4-2. Owner resolution in the stand-in census (`last_standin_call`)

**What.** The census matches a call's constant-pool owner against the row's class literally. javac
qualifies an unqualified call by the enclosing class (JLS 13.1), so `wait(delay)` inside
`Thread.join(long)` names `java/lang/Thread.wait(J)V`, and a JDK refactor that moves a census method
up or down a hierarchy silently turns a chain off. Resolve the owner and walk up to the row's class
(`standin_declaring_class` already does it for the ENTRY call; the inner hops do not). **Benefit:**
robustness against JDK updates; the prerequisite for any `join` row (item 1 of
`r13w13-trace3-vm-served-jdk-methods-residuals`). **Cost:** small. **Risk:** a hop that resolves to a
subclass override -- refuse any class that declares the method itself, as the entry hop does.
**First step:** a unit test with a `Thread` fixture whose `join(J)V` calls `Thread.wait(J)V`.

## Round 14 wave 4 (lane trace3): T4-2 landed

`vm/src/runtime/stackwalker.rs` `last_standin_call` now takes a `reaches(owner, row)` resolver:
every call matching a row by name / descriptor / screen whose constant-pool owner resolves up the
superclass chain to the row's class (`standin_declaring_class`, which refuses a class on the way that
declares the method itself) is a census call; the literal-owner path is unchanged. The chain loop
(`standin_chain`) moves to the resolved class id. Switch `CRATONVM_THROWABLE_STANDIN_OWNER_RESOLUTION`
(default on; read only for a non-literal owner). First user: the `join()` rows
(`r13w13-trace3-vm-served-jdk-methods-residuals` item 1). Unit test
`r14w4_trace3_tests::a_registered_join_gets_the_wait_chain_through_owner_resolution`.

## T4-3. Leaf frames in a parked thread's PUBLISHED stack

**What.** `Thread.getStackTrace()` of a thread parked in a registered `sleep` / `wait` shows its caller
on top where HotSpot shows `Thread.sleepNanos0` / `Object.wait0` (item 6's original half). The
blocking natives know exactly which leaf they are in when they deposit the published trace
(`deposit_root_snapshot` sites): let the deposit record a one-byte "served leaf" tag (sleep / wait /
park) next to the snapshot, and let the reader expand it with `native_standin_frames`' chain builder,
using the tag instead of a throwable screen. **Benefit:** thread dumps and sampling profilers match
HotSpot for the most common parked shapes. **Cost:** medium (a field on the published trace, the three
deposit sites, the reader). **Risk:** a stale tag after unblock -- clear it in the same unblock that
marks the thread runnable. **First step:** census which deposit sites are reached from the registered
`sleep` / `wait` / `park` natives.

## Round 14 wave 4 (lane trace3): T4-3 landed

Landed without the tag (the deposit sites and the published-trace field are outside the lane): the
screen is the thread's CURRENT block state. `vm_exec.rs` `append_parked_standin_entries`, called at
the end of `thread_stack_trace`'s other-thread branch (so `Thread.getStackTrace()` of another
thread, `getAllStackTraces` / dumps and the JMX `ThreadInfo` all get it), appends
`stackwalker::parked_thread_standin_frames` when `java_block_state` is `WAITING` / `TIMED_WAITING`
(the trace read is then the one the blocking native deposited, standing at its invoke), the thread
object is not a virtual thread's (`is_virtual_thread_class`), and the published innermost entry
stands at a call of a census ENTRY row (the method found by name plus "exactly one overload whose
code at the bci is such a call", since published entries carry no descriptor). The chain is the
throwable census's (`standin_chain`) with a `Parked` screen admitting every row. `BLOCKED` (the
monitor re-acquire after `wait`) is left out; a thread that unblocks between the two reads shows
where it was parked. Switch `CRATONVM_THREAD_STACK_PARKED_LEAF_FRAMES` (default on, both modes).
Tests `r14w4_trace3_tests::a_parked_threads_published_stack_gets_the_leaf_frames`; probe
`C:\craton\jitr14-probes\src\R14Trace3ParkedLeaf.java`. The JMX locked-monitor depths need the
patch page `r14w4-trace3-jmx-locked-monitor-depths-after-served-frames-patch-FIXED-20260929.md` (wave 3's
`Thread.run` frames already broke them). Not covered: `LockSupport.park` (not a census method; its
leaf is `Unsafe.park`, a different census) and a registered `sleep`'s 10 ms pump gaps, where the
thread reads `RUNNABLE` for microseconds and gets no leaf.

## T4-4. `Thread.runWith` for `SHOW_HIDDEN_FRAMES` walkers

**What.** HotSpot shows the `@Hidden` `Thread.runWith` frame between the task and `Thread.run` to a
walker with `StackWalker.Option.SHOW_HIDDEN_FRAMES`. The walk stand-in adds `Thread.run` only. Add
`runWith` (at its only `run()V` call) in the walk when `--jdk-only` (where the hidden-frame filter
drops it for ordinary walkers); never in a throwable. **Benefit:** exactness for debuggers / agents
that show hidden frames. **Cost:** small. **Risk:** `--compatible` hides nothing, so it must stay
off there. **First step:** a probe row with `SHOW_HIDDEN_FRAMES` on HotSpot 25.
