# JIT round 14 proposals, lane trace3 (wave 4)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: VM-served JDK frames in stack captures (throwables, other threads' stacks, JMX)
Found by: round 14 wave 4 lane trace3

Ranked by expected benefit over cost. Earlier books: `jit-r14-trace-proposals.md`,
`jit-r14-trace3-proposals.md`, `jit-r14-trace4-proposals.md`.

## TR5-1. Apply the JMX locked-monitor depth patch, then pin it with a unit test

**What.** `r14w4-trace3-jmx-locked-monitor-depths-after-served-frames-patch-FIXED-20260929.md`: since wave 3,
`ThreadInfo.getLockedMonitors()` reports every monitor at depth 0 for a started thread with a task,
because `thread_jmx_snapshot` matches trace lengths. After the patch, a `vm` unit test over
`add_thread_run_walk_entries`' insertion list and the index shift (`index + |{at <= index}|`) keeps the
next served frame from breaking it silently. **Benefit:** correct `- locked` placement in every JMX
thread dump (JConsole, VisualVM, Spring Actuator's `threaddump`). **Cost:** small. **Risk:** low.
**First step:** apply the page's two hunks.

## TR5-2. A per-thread memo of the bottom `Thread.run` decision

**What.** The capture of a throw on an executor worker (`ThreadPoolExecutor$Worker.run` outermost)
still takes the class-manager read lock and walks two fields (`target`, `holder.task`) and the task's
`run()V` dispatch on EVERY throw, although the answer depends only on (thread object, outermost frame
class): `target` and `holder.task` are final. A `JvmThread` field `thread_run_bottom: Option<(ClassId,
Option<StackTraceEntry>)>` keyed by the outermost frame's class id, filled on the first throw,
cleared on class redefinition (`redefinitions_seen` already exists there), takes every later throw off
the lock. **Benefit:** servers throw on pool threads constantly (Tomcat/Netty handlers that use
exceptions for control flow); the lock is shared with class loading. **Cost:** small (a field in
`jvm_thread.rs`, interpreter-round file; the check in `prepend_thread_run_standin_frame`). **Risk:** a
stale entry after redefinition -- key it on `redefinitions_seen`. **First step:** count captures that
reach `thread_run_bottom_frame` on a Tomcat request loop (`CRATONVM_DBG_STTRACE`).

## Round 14 wave 6 (lane trace5): TR5-2 landed

`JvmThread::thread_run_bottom_memo` (`stackwalker::ThreadRunBottomMemo`, keyed by the
process redefinition count, the `Thread` object's class and the outermost frame's class id / name /
descriptor; a frame without a class id is never memoised), consulted in
`vm_exec.rs` `prepend_thread_run_standin_frame` before the class-manager lock. Switch
`CRATONVM_THROWABLE_THREAD_RUN_MEMO` (default on; `0` never fills it). Details on
`r14w5-trace4-thread-run-memo-and-timed-join-hint-patch-FIXED-20260929.md`.

## TR5-3. A served-call hint for argument-dependent chains (timed `join`)

**What.** An interrupted DIRECT `t.join(ms)` under `--compatible` still gets no JDK frames: HotSpot
leaves `join(long)` from its timed `wait(delay)` (line 1881) when `ms > 0` and from the `wait(0)` loop
(1887) otherwise, and the frames alone cannot tell which. The registered native knows `ms`: let it set
a one-shot per-thread hint ("the served `join(J)V` ran with a positive timeout") that the capture
consumes, and let the census pick the FIRST vs LAST call site from it. **Benefit:** exact timed-join
traces; the same mechanism serves `Object.wait(long, int)` / `Thread.sleep(long, int)` should a JDK
split their sites. **Cost:** small-medium (a `JvmThread` byte, set in two natives, read in
`append_native_standin_frames`). **Risk:** a hint left set by a native that returned normally -- clear
it on every exit of the native. **First step:** `R14Trace3JoinFrames` `joinTimed` row.

## Round 14 wave 6 (lane trace5): TR5-3 landed

Not by first-vs-last: JDK 17 orders `join(long)`'s loops the other way round. The `join(long)` hop
now picks its `wait` by ARGUMENT (`lconst_0` = the `wait(0)` loop), which also fixed the wave-4
`join()` row on JDK 17. The hint (`NativeThreadAccess::set_served_timed_join_hint`,
`JvmThread::served_timed_join_hint`) is set only on the `InterruptedException` return of the served
`join(long)` / `join(long, int)` and taken by every capture. Switch
`CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES` (default on). Details on
`r14w5-trace4-thread-run-memo-and-timed-join-hint-patch-FIXED-20260929.md`.

## TR5-4. Leaf frames for `BLOCKED`-in-`wait` and for the registered sleep's pump gaps

**What.** The wave-4 parked-leaf screen (T4-3 landed with the thread's block state, not a tag) misses
(a) a thread re-acquiring its monitor after `notify` (`BLOCKED`; HotSpot still shows `Object.wait0`)
and (b) a registered `sleep` caught in the microseconds between its 10 ms `TIMED_WAITING` slices
(the cross-thread read then publishes afresh and finds `RUNNABLE`). The original T4-3 form -- a
one-byte "served leaf" tag the blocking native sets for its whole call and clears on exit -- covers
both. **Benefit:** deterministic thread dumps of sleepers. **Cost:** medium (the tag on `JvmThread` or
the registry entry, set in `native_thread_sleep` / the wait path, read in
`append_parked_standin_entries`). **Risk:** a tag left behind on an unwinding exit; set/clear in a drop
guard. **First step:** measure how often `R14Trace3ParkedLeaf`'s `sleeper` row needs its retry loop.

## TR5-5. `LockSupport.park` in parked stacks

**What.** The most common parked shape in a server dump is `Unsafe.park(Native Method)` under
`LockSupport.park` (every `ThreadPoolExecutor` idle worker). Under `--jdk-only` the published innermost
frame is `LockSupport.park` standing at `Unsafe.park`; if the VM serves `Unsafe.park` without a frame
(it is a native either way), the leaf is missing exactly as `wait0` was. Add `jdk/internal/misc/Unsafe.park(ZJ)V`
(native leaf) as a Parked-only census row (no throwable: `park` raises nothing). **Benefit:** idle
pool threads read like HotSpot's. **Cost:** small (one row plus a "parked-only" flag).
**Risk:** a registered `LockSupport.park` under `--compatible` with a different chain -- the chain is
read from the real bytes, so only the entry row matters. **First step:** a probe row with an idle
`Executors.newFixedThreadPool(1)` worker's `getStackTrace()` on HotSpot 25.
