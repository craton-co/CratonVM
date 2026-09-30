# Proposal: per-thread handshakes instead of global pauses for non-GC operations

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 36
> of 54).** Not built. The JIT poll still tests the global byte. Shares the
> per-thread poll word with the conc6 page's owed bit; the interpreter already
> has one (`LoopPollWord`), and gcd d5/f added a registered per-thread word
> with a drain under `inner` (`GcBarrier::drain_leaf_windows`), a reusable
> pattern. **Gate:** `door=frame-trace` pause lines (`--verbose:gc`) before
> and after each staged step on a sampler workload. **Size:** M (steps 1-2), L
> (step 3, JIT round).

> **Design refresh, gc-common w6-a (2026-09-24): still accurate, not
> advanced. The JIT poll encoding belongs to the JIT round. There is one new
> argument for the proposal.**
>
> Every production pause now takes the process coverage slot. That includes
> the `door=frame-trace` pause behind a cross-thread `getStackTrace()`. It
> has to: its request clears the process-global per-pause rows
> (`reset_peer_proven_jit_depth`) just as a collection's does
> (`common-a-process-global-gc-coordination-state.md`, w6-a). So in a
> process with two VMs, a stack sample in VM B now waits for VM A's pause to
> end (bounded by `COVERAGE_SLOT_WAIT_LIMIT`, 250 ms). A handshake would
> neither stop the world nor touch those rows, so it would not need the slot.
>
> The staging is unchanged. It can be done without the JIT change, in this
> order:
>
> 1. A handshake queue per `JvmThread` (closure plus done-flag), drained at
>    `safepoint_check` and by the requester for a target already inside
>    `in_blocked_region`. This is the cross-thread stack-trace fast path the
>    code already has for parked targets.
> 2. Stack sampling of an interpreted or blocked target moves to that queue.
>    Only a target that is in compiled code keeps the pause.
> 3. The per-thread poll byte, which needs the JIT round.
>
> Measure each step on the `door=frame-trace` pause lines (`--verbose:gc`).

> **Re-verified gc-common w5-a (2026-09-23): still accurate, not advanced**
> (the JIT poll encoding is the JIT round's). The proposal is now
> MEASURABLE, though. Under `--verbose:gc` every cross-thread
> `getStackTrace()` pause prints its own `[GC] pause: gc=- door=frame-trace
> ttsp_us=... work_us=...` line (`common-w4a-non-collection-pauses-have-no-pause-line`,
> FIXED in w5-a). So the cost this proposal would remove (a full-world TTSP
> per sample) can be read per pause on a real sampler, which is the number
> to take before and after.


> **Re-verified gc-common w4-a (2026-09-23): still accurate, not advanced.**
> The cheaper step named below landed: `FRAME_TRACE_WANTED` is a counter.
> A cross-thread `getStackTrace()` still stops the world, now with the same
> identity census as a collection (`request_non_collection_pause`).

Status: PROPOSAL (filed gc-common round 2026-09-23, wave 1, lane A)
Area: safepoints / STW protocol

> **Re-verified gc-common w3-a (2026-09-23): still accurate, not advanced**
> (the JIT poll encoding is the JIT round's). One cheaper step is filed in
> the meantime: `FRAME_TRACE_WANTED` as a counter
> (`docs/internal/gc-common-round-20260923/applied/handoff-w3a-frame-trace-wanted-counter.md`),
> which fixes two concurrent `getStackTrace()` callers clobbering each other's
> request without changing the global-pause design.

## Today

Every operation that needs a thread to be at a known point stops the WORLD:

- cross-thread `Thread.getStackTrace()` / `dumpThreads()` —
  `stw_publish_frame_traces` (`vm/src/runtime/interpreter/gc_and_alloc.rs:260`)
  takes a full pause (including an OS-level JIT takeover pass and a TLAB
  skip-span publish) to read ONE thread's frames;
- `Thread.stop` / async exceptions need no pause (the per-thread
  `async_exception_slot` that polled for them had no producer and was removed
  in interpreter round i1 wave 4 — see
  `docs/internal/fixed-bugs/interpreter-L1-async-exception-channel-is-dead-FIXED-20260923.md`;
  a future JVMTI `StopThread` is a natural first handshake client);
- biased/lightweight lock revocation, deoptimisation requests and code-cache
  withdrawal piggy-back on GC pauses or their own polls.

`stw_publish_frame_traces`'s doc names the reason: the JIT poll tests exactly
one byte, the barrier's `stw_requested`, and widening that costs every back
edge.

## Proposal

HotSpot's JEP 312 shape, adapted to the existing poll:

- keep ONE polled byte, but make it per-THREAD (`JvmThread.poll_word`) with
  the global barrier setting every thread's byte (O(threads) stores per
  pause, paid by the initiator instead of an extra load per back edge); the
  JIT already addresses the thread through a register, so `TEST BYTE
  [thread+off]` is the same 4-byte instruction class as today's
  RIP-relative form;
- a handshake sets one target's byte and queues a closure; the target runs
  it at its next poll and clears the byte; a blocked target runs it on the
  requester's side under its `in_blocked_region` guarantee (frames are
  quiescent), exactly as the cross-thread stack-trace fast path already
  does for parked targets.

Wins: `getStackTrace` on a running thread stops one thread, not all; the
takeover machinery stays GC-only; `FRAME_TRACE_WANTED` (a process global)
goes away.

Risk: the JIT poll encoding changes (out of scope for this round; needs the
JIT round). Measure `PollBench` before/after.
