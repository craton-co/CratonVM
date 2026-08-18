# `Thread.getStackTrace()` on a running thread returned an empty array — FIXED

| | |
|---|---|
| **Status** | **FIXED 2026-08-18**, found the same day while profiling H2. |
| **Symptom** | `t.getStackTrace()` for any `t` other than the caller returned a **zero-length** `StackTraceElement[]` whenever `t` was actually executing Java code. Frames came back only when the target was parked, blocked or sleeping. |
| **Cause** | `JvmThread::frame_trace` — the only thing a cross-thread reader can see — is written at the **blocking deposit points** and nowhere else. A thread that has never blocked has published nothing; one that has publishes where it blocked *last*. |
| **Fix** | Ask the target to publish, by taking the safepoint that makes it. Reader side skips the pause when the target is already parked. |

## What it looked like

`StackProbe.java` (committed under `probes/`): a daemon sampler calls
`main.getStackTrace()` every 2 ms while the main thread spins through three
named methods, and a `volatile phase` records which one is running.

| | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `alpha` | 259 | **0** | 485 |
| `beta` | 268 | **0** | 485 |
| `gamma` | 272 | **0** | 496 |
| `<empty>` | 0 | **1471** | 0 |

After the fix the per-phase split is exact — every `phase=1` sample names
`alpha`, every `phase=2` names `beta`, every `phase=3` names `gamma` — on
**ZGC, G1 and Serial, with the JIT on and with `--nojit`**.

`Thread.getAllStackTraces()` (the `dumpThreads` path) was broken the same way
and is fixed with it: for a running target, `frames=0 top=<empty>` →
`frames=2 top=StackProbe2.busy`, matching HotSpot's top frame.

## Why it happened

`frame_trace` is an `Arc<Mutex<Vec<StackTraceEntry>>>` shared between the
`JvmThread` and its `ThreadRegistry` entry — that sharing is what lets another
thread read it at all, because `JvmThread::frames` is owned by its own thread
and cannot be walked from outside. The only writer was
`deposit_root_snapshot_inner`, which runs at the **blocking** deposit points.

For a parked thread that is exactly right, and it is what the field was built
for: the deposit shows the blocking call site. For a running thread it produces
one of two wrong answers, and the second is worse than the first:

* **never blocked** → the vector is empty → a zero-length array;
* **blocked earlier** → the vector holds a stack from some past blocking call →
  a confident, plausible, wrong answer.

Both were observed in the same run: rounds 1 and 2 of the overhead probe
returned 736 *non-empty* samples from the unfixed binary, every one of them
stale.

## The fix

A running thread's stack can only be published by that thread, at a point where
it is not mid-instruction. That is a safepoint, so:

1. **`stw_publish_frame_traces`** (`gc_and_alloc.rs`) takes a stop-the-world
   pause whose entire purpose is that every arriving mutator publishes. It reuses
   the GC's `request_stw_counted_with_live_blocked` + `stw_take_over_and_wait` +
   `complete_gc` sequence — including the take-over, so a peer spinning in
   compiled code that never reaches a poll is frozen rather than waited on
   forever. Nothing moves, so there is no pointer map.
2. **`safepoint_check`** publishes `capture_frames_no_lines(&thread.frames)`
   into `thread.frame_trace` on its way into the pause, next to the root
   snapshot it already deposits there — **gated on the request flag**. That
   matters: this code runs on every mutator on every GC pause and the capture
   allocates a `Vec` per thread. Thread dumps are rare, GC pauses are not; unset,
   the gate is one relaxed load.
3. **`NativeContextImpl::thread_stack_trace`** takes that pause before reading —
   and **only when the target is not blocked**. A parked target's deposit is
   already current, so pausing the world to re-derive a stack we have would be
   pure cost, and that is the common case for a thread dump or a deadlock
   detector.

Reading the **current** thread is unchanged and still walks live frames
directly — no pause, no flag.

## What it costs

Sampling a running thread every 2 ms, interleaved rounds of the same warmed loop
with the sampler off and on:

| | ratio (on / off) |
|---|---|
| HotSpot 25 | 0.99 – 1.03 |
| CratonVM after | **1.02 – 1.05** |

The unfixed VM measured 1.00 – 1.01, which was free because it did no work and
returned nothing.

HotSpot uses a per-thread *handshake* rather than a global pause, which is
cheaper. That option is not open here without cost on the hot path:
`emit_safepoint_poll` tests exactly one byte — the GC barrier's `stw_requested`
— and widening that test would land on every back-edge of every compiled method.
Reusing the existing pause keeps the change entirely off the hot path.

## Known residuals, not introduced here

* **`Thread.getAllStackTraces()` takes one pause per running thread.** The
  `dumpThreads` native loops calling `thread_stack_trace`, so an N-thread dump
  costs up to N pauses where HotSpot takes one. Correct, and N times more
  expensive than it needs to be; batching it needs the loop to take the pause
  once, which is a native-API change. The alternative it replaces returned
  nothing at all.
* **A parked thread's trace is short.** For a thread inside `Object.wait()` this
  VM reports 1 frame (`StackProbe2.lambda$main$0`) where HotSpot reports 4,
  topped by `Object.wait0`. That is the blocking-deposit capture's own depth, is
  unchanged by this fix, and is a separate gap.
* **`getAllStackTraces()` returns fewer threads than HotSpot** (3 vs 8 on the
  probe) — VM-internal threads HotSpot exposes and this one does not. Also
  pre-existing.

## Why this was worth fixing beyond the API contract

It silently disables every in-process sampling profiler, thread-dump utility and
hang/deadlock diagnostic that inspects another thread — and it does not fail
loudly, it fabricates. It was found because a sampler built on it reported, with
complete confidence, that 100% of an H2 merge batch was spent in a
connection-close path that was **outside the sampled window entirely**: every
in-window sample came back empty and was dropped, leaving only the samples taken
while the target was blocked. The wrong profile survived until a standalone
probe checked the instrument itself against HotSpot.

## Reproduction

```bash
/data/toolchain/jdk-25/bin/javac -d /tmp/sw probes/StackProbe.java
# control
/data/toolchain/jdk-25/bin/java -cp /tmp/sw StackProbe 40000000
# CratonVM
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> --java-home /data/toolchain/jdk-25 \
    --Xmx 1g -XX:+UseZGC -c /tmp/sw StackProbe 4000000
```

`probes/StackProbe2.java` covers the parked target and `getAllStackTraces`;
`probes/Overhead.java` is the interleaved sampler-off/sampler-on cost
measurement.
