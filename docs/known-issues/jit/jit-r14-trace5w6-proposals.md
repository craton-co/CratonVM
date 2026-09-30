# JIT round 14 proposals, lane trace5 (wave 6)

Ranked. Each builds on what wave 6 landed (TR5-2 memo, TR5-3 served-join hint).

## TW6-1. Reuse the `Thread.run` memo for the current thread's `StackWalker` walk

**What.** `vm_exec.rs` `add_thread_run_walk_entries` calls `thread_run_bottom_frame` (class-manager
lock already held there, plus the two field walks and the lambda-table probe) on every walk of the
current platform thread whose outermost frame passes the screens. Log4j2 / SLF4J caller lookup
(`StackWalker.getCallerClass`, `walk`) runs it per log call on pool threads. The walk has the
current `JvmThread`; pass `&mut thread.thread_run_bottom_memo` there too (the key is the same:
the entry becomes a `BacktraceFrame::Entry`). **Benefit:** per-log-call cost on executor threads.
**Cost:** small (the walk's caller must reach the `JvmThread`; check which walk paths are
current-thread only). **Risk:** a walk of ANOTHER thread must not use or fill this thread's memo.
**First step:** confirm `add_thread_run_walk_entries` callers pass only the current thread's object.

## TW6-2. Virtual-target joins decline the `wait` chain

**What.** A registered `join()` / `join(long)` whose TARGET is a virtual thread gets the platform
`Object.wait` chain; HotSpot shows `VirtualThread.joinNanos`. The natives can read the target's
class: when it is a `BaseVirtualThread` / `VirtualThread`, set a "no chain" hint
(`served_timed_join_hint` could become a small enum: `Timed`, `Zero`, `Decline`) that the census
honours for the `join()V` entry too. **Benefit:** no fabricated frames. **Cost:** small. **Risk:**
the untimed `join()V` row today needs no hint; the decline must be consumed by the same capture.
**First step:** a probe joining a virtual thread under `--compatible`, interrupted.

## Round 14 wave 7 (lane trace6): TW6-2 landed

Narrowed by reading: a virtual thread here is a `BoundVirtualThread`, for which JDK 25
`join(long)` (`this instanceof VirtualThread`) runs the platform `wait` loops, so the census rows
are right for it and are kept. The decline applies to a `java.lang.VirtualThread` target only
(`lang_system.rs` `join_target_runs_join_nanos`): the served `join()` leaves hint `true` (`false`
otherwise, always, so no stale hint reaches it) and the capture drops the chain
(`stackwalker::served_join_declines_standin_frames`); the timed joins leave no hint. No enum was
needed: the bool is read by call and throwable. Switch
`CRATONVM_THROWABLE_STANDIN_VIRTUAL_JOIN_DECLINE`. Details on
`r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md` (wave 7).

## TW6-3. An argument-check hint for `join(long, int)`'s two `IllegalArgumentException` sites

**What.** `join(-1, 0)` / `join(0, 1_000_000)` raise IAE at two different sites in the JDK body; the
one-frame argument-check rule refuses a body with two sites. The served native knows which check
failed; the same one-shot hint mechanism can name the site ordinal (first / second `new IAE`).
Also applies to `Thread.sleep(long, int)` and `Object.wait(long, int)` if registered.
**Benefit:** exact IAE traces. **Cost:** small. **Risk:** low (fail-closed as today when absent).
**First step:** census of registered `(JI)V` natives and their JDK bodies' IAE site counts.

## Round 14 wave 7 (lane trace6): TW6-3 landed

Census: `Thread.join(JI)V` and `Thread.sleep(JI)V` are registered on a real JDK (two IAE sites each,
JDK 17/21/25); `Object.wait(JI)V` only by `register_synthetic_overrides` (not a row). The natives
leave the site bit through `served_two_site_argument_check` (`lang_system.rs`); the capture takes
the frame at that site (`stackwalker::served_arg_check_standin_frames`,
`STANDIN_ARG_CHECK_TWO_SITE_METHODS`). Switch `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_SITE_HINT`.
Probe `R14Trace6ArgCheckSites`.

## TW6-4. A capture-cost census line for the memo

**What.** `CRATONVM_DBG_STTRACE` could print one line per capture saying whether the bottom frame
came from the memo, a miss, or a screen refusal, so the orchestrator can confirm TR5-2 on a Tomcat
request loop without timing. **Benefit:** measurable claim. **Cost:** tiny (a `dbg_sttrace()` guarded
`eprintln!` next to the existing `STTRACE_DBG_CAP` lines; the diag-print gate already admits that
family). **Risk:** none. **First step:** add it next to `STTRACE_DBG_CAP`.
