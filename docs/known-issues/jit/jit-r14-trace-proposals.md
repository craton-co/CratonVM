# JIT round 14 proposals, lane trace (wave 2)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: stack-trace capture through compiled frames, `StackTraceElement` fill, VM-served JDK frames
Found by: round 14 wave 2 lane trace

Ranked by expected benefit over cost.

## T14-1. The innermost compiled frame's exact return address, for both tiers

**What.** A compiled frame whose callee runs in the interpreter, a native or a runtime helper is
placed today by its safepoint-id slot (key 2); only a frame with a compiled callee has its exact
return address (key 1). Wave 2 made the optimizing tier's key 2 exact by recording chains per
safepoint id (`r14w2-trace-conservative-roots-ir-innermost-chain-by-sp-id-patch`); the
single-pass tier still keys by `cur_bc_pc`, which is poisoned by any splice with two calls and by
miss edges. Recording the return address at the helper boundary (the helpers already publish
`TOP_RBP`; one more word, the caller's return address read from `[rsp]` on helper entry, or the
`not_entrant_ret_pc` mechanism generalised) would give BOTH tiers key 1 for every frame.
**Benefit:** no spliced frame is ever dropped from a trace, whichever tier and whichever callee
kind; the per-id table can then go. **Cost:** medium (every helper entry that can run Java; one
store each). **Risk:** a stale word naming the previous call -- must be cleared on return, like
`not_entrant_ret_pc`. **First step:** census with `CRATONVM_DBG_SWCHAIN=1` how many captures reach
key 2 on `R14TraceOptDrift` and on the Spring battery, per tier.

## T14-2. `Thread.run()` as real bytecode (the bottom frame of every started thread)

**What.** `Thread.run()V` is a registered Bridge AND force-listed
(`native_override.rs::force_native_over_real_jdk_bytecode`), so an exception escaping a Runnable
has no `java.base/java.lang.Thread.run(Thread.java:1474)` frame in either mode
(`R13Trace3StandinGaps` `runnable`). JDK 25's body (`holder.task`, then the `@Hidden runWith`) is
layout-stable under `--jdk-only`. Retire the Bridge through `retired_shadow.rs` under
`--jdk-only` first, gate the force arm on the same switch, keep both for `--compatible` until the
WildFly `JBossThread` shape is re-measured. **Benefit:** the bottom frame of every thread trace,
`Thread.getStackTrace()` of other threads, and one fewer Bridge over bytecode. **Cost:** small
code, medium measurement (uncaught-exception handler path, virtual threads, JBoss). **Risk:** a
VM-created thread whose `holder.task` is unset. **First step:** run `R13Trace3StandinGaps` and
the Tomcat/WildFly drivers with `CRATONVM_UNRETIRE_NATIVE_SHADOW` scoped to `Thread.run`.

## T14-3. Stand-in frames for other threads' stacks

**What.** The stand-in census applies to throwable captures only; `Thread.getStackTrace()` of a
thread blocked in a registered `sleep` / `wait`, `jstack`-style dumps and
`ThreadMXBean.getThreadInfo` show the caller on top where HotSpot shows `Thread.sleepNanos0` /
`Object.wait0` (vm-served page item 6). The same `native_standin_frames` call, with no throwable
screen (the thread IS inside the leaf), at `threading::thread_registry`'s capture. **Benefit:**
thread dumps that match HotSpot's (monitoring tools key on the top frame). **Cost:** small.
**Risk:** a thread that has left the leaf between the park flag and the walk -- read the flag
the walk already uses to decide "blocked". **First step:** a probe dumping a sleeping thread's
`getStackTrace()[0]`.

## T14-4. The JDK's hashed-module set instead of a name screen

**What.** `is_jdk_module_name` (`java.*` / `jdk.*`) stands for `isHashedInJavaBase`; the real set
is the `ModuleHashes` attribute of `java.base`'s `module-info`. Read it once per VM when the image
is loaded and use it for the element's `format` bit and the printed version. **Benefit:** exact
`computeFormat` parity (upgradeable `java.compiler` prints its version on HotSpot). **Cost:**
small. **Risk:** none beyond the read. **First step:** check whether the module reader keeps the
`ModuleHashes` attribute.

## T14-5. A trace-parity debug arm for the JIT

**What.** `CRATONVM_DBG_TRACE_TIER_PARITY=1`: on every throwable capture that went through a
compiled frame, also rebuild the trace from each compiled activation's deopt metadata (the frame
states the deopt path already materialises) and count / print the first disagreement with the
walk. Round 13's single `bad 1` took a probe, three arms and a wave to localise; this names the
activation and the key that answered. **Benefit:** every trace drift becomes a one-run diagnosis.
**Cost:** medium (debug only). **Risk:** none in production (off). **First step:** reuse
`jit::deopt::ReconstructedFrame` for the innermost activation only.
