# Graph capture — replaying a launch sequence as one submission

A kernel launch costs the host something whether or not the device is
busy. A workload that issues hundreds of small launches per unit of work
pays that cost hundreds of times, and past some point the device
finishes each kernel before the host has finished asking for the next
one. Nothing about making an individual launch cheaper fixes that: the
cost is *per launch*, and the launches are what there are too many of.

CUDA graphs turn a recorded sequence into one object the driver already
knows the shape of. Capture records launches instead of issuing them;
instantiation resolves them once; replay is a single `cuGraphLaunch`.

## The measurement that motivated it

GPULlama3's decode step is 453 launches on one stream, 12 distinct
kernels. On an RTX 2060:

| | host time to submit | device time left over |
|---|---|---|
| idle box | 14 ms | 24 ms |
| loaded box | 60 ms | 5 ms |

The second row is the whole argument. Under load the device is starved:
it spends 5 ms working and the rest waiting for the host.

`cuda-bridge/tests/graph_capture_it.rs` prices the ceiling directly, on
the same 453 launches with a kernel small enough that the launch
dominates it:

    issue  host 4.03 ms / total 4.03 ms
    replay host 0.55 ms / total 0.65 ms      host cost 7.3x lower

## What it bought, end to end

Three arms of the same binary and the same jar, alternating within each
round, on a host gated quiet by `bench-gpu/wait-for-quiet.sh`
(`bench-gpu/run-gpullama3-graph-ab.sh`):

| arm | best tok/s | rounds |
|---|---:|---|
| `base` — per-token scalars as kernel arguments | 16.68 | 8.68 7.71 16.68 14.08 14.93 |
| `scalars` — those two ints device-resident, graph off | 15.14 | 9.21 15.14 14.21 14.94 13.60 |
| `graph` — the same, graph on | **31.43** | 15.09 27.74 30.19 31.43 30.86 |

**2.05x**, and all three arms produce byte-identical text. `submit_ms`
per token falls from ~117 ms to 0.5–0.8 ms. The first round of every arm
is cold, which is why the rounds are printed rather than only a mean.

The middle arm is there because it is the one that could have gone
wrong: moving the position and the token id into device memory adds a
global load per thread to six kernels, and if that cost anything the
graph would have been paying for it. It does not.

## Why the scalars had to move

A graph bakes each argument *value* into its nodes at capture time, so
an argument that changes between replays is exactly what stops a
sequence being replayable. In GPULlama3 only two things change per
token — the sequence position and the token id — and they appear in
about 113 of the 453 launches.

Moving them into a two-element device-resident `int[]` that the kernels
index makes the graph invariant. The host writes the new values through
the pointer the graph already holds (`GpuArray.copyFromHost` →
`Native.arrayCopyFromHost` → `DeviceBuffer::copy_from_host`) and
replays.

This worked for that application because **no parallel loop bound
depends on the position** — every kernel's grid is sized from some
parameter array's length. A workload whose *grid* changes per iteration
cannot be captured this way at all; its launch configuration is part of
the node, not part of the data.

## The rules, and who enforces them

Three things are true inside a capture and nowhere else. The Java API
documents all three on `GpuExecutor.beginCapture`, because they are the
caller's to respect:

1. **Nothing runs.** A dispatch made while capturing returns no usable
   submission handle, and waiting on one waits forever.
2. **Nothing may ask the device a question.** Synchronising, polling a
   future, reading an array back: any of these invalidates the capture.
   `endCapture` reports that rather than handing back a graph that runs
   and does nothing.
3. **Device memory must outlive the graph.**

The VM enforces what it can:

* **Refusal.** A dispatch whose writeback targets a plain Java array is
  refused by name and the capture fails. Such an array is marshalled
  into a fresh device buffer per dispatch, so capturing one bakes in a
  pointer that is freed before the first replay — a silent wrong answer.
  A resident `GpuArray` keeps its buffer for the life of the process.
* **Node count.** `graphNodeCount` is checked against the number of
  dispatches made. A graph with fewer nodes than the loop had launches
  replays successfully and does *less*, which is the worst way for any
  of this to fail.
* **Pinning.** A captured dispatch never finalizes, so the device
  buffers its `FinalizeState` owns are kept for the life of the graph —
  and the GC-critical guard in that same state is dropped, because
  holding 453 of those would stop the collector for the life of the
  process.
* **Staleness.** A replay runs kernels and no writebacks, so the
  resident handles the capture recorded are marked dirty per replay.
  Without that, `GpuArray.toHost` answers with the host mirror from
  before the replay.
* **The bounds flag.** Every dispatch is normally given its own
  one-cell buffer that its kernel sets on an out-of-range index. A
  capture shares *one* cell across all its launches, so a replay reads a
  single cell instead of 453 — per-node host work is the thing being
  removed. It is allocated before the capture opens, because a
  `cuMemAlloc` inside one invalidates it, and it is sticky: nothing
  resets it between replays.

## The `last_write` interaction

`launch_raw_on_stream_inner` stamps every device-pointer argument's
`last_write` slot with the kernel's completion event, and a subsequent
`to_host` waits on that event before its D→H copy. An event recorded on
a *capturing* stream exists only inside the graph, so a stream that
later waits on it fails with `CUDA_ERROR_INVALID_VALUE` — which is how
this first showed up, on the one buffer the application reads back.

The launch path therefore asks the stream whether it is capturing (a
plain flag set by `begin_capture`, not a driver query on every launch)
and, if so:

* skips the waits — a single-stream capture becomes a linear chain of
  nodes whose dependencies the driver derives from submission order, so
  the ordering is already there; and
* **clears** the slots rather than stamping them. Leaving the old event
  would be worse than clearing: it describes a write that happened
  before this graph, so a download released by it would be correctly
  ordered against the wrong thing.

Ordering across a replay is therefore the caller's wait on the replay's
submission handle, which is why `replay` returns one.

## Using it

```java
exec.beginCapture();
for (Step s : steps) s.dispatch();          // recorded, not run
long g = exec.endCapture();
if (exec.graphNodeCount(g) != steps.size()) { /* refuse it */ }

for (int i = 0; i < many; i++) {
    scalars.copyFromHost(perIteration);     // through the baked-in pointer
    exec.awaitSubmission(exec.replay(g));   // one submission, all of them
}
exec.releaseGraph(g);
```

`craton_gpu` natives: `graphBeginCapture(J)Z`, `graphEndCapture(J)J`,
`graphReplay(JJ)J`, `graphNodeCount(J)I`, `releaseGraph(J)V`, and
`arrayCopyFromHost(JLjava/lang/Object;)Z`. The first three take the
*executor* handle — a capture belongs to the stream every dispatch on
that executor lands on.

## Related

* [`streams-events.md`](streams-events.md) — the `last_write` event
  discipline this suspends.
* [`async-api.md`](async-api.md) — submission handles, awaiting and
  releasing.
* [`occupancy-launch-config.md`](occupancy-launch-config.md) — where a
  captured node's block size comes from.
