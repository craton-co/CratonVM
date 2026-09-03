# The GPU submission registry has no drain: every async submission leaks its entry for the life of the process

**Status:** OPEN. **Read off the source, not off a run** — this host has no CUDA
device and `gpu-offload` is not a default feature, so the evidence below is the
call graph, and it is exhaustive rather than sampled.

`vm/src/runtime/offload.rs`'s `SUBMISSIONS` map has **exactly one insert and
exactly one remove**:

```
3805:        let mut table = submissions().write();
3806-        table.insert(h, sub);          // register_submission
3847:    submissions().write().remove(&handle);   // release_submission — the ONLY remove
```

and since `b6133b92d` (2026-09-02, "the four review items") `release_submission`
has **no production caller anywhere in the workspace**. Its only references are
`vm/tests/gpu_offload_features.rs` and `#[cfg(test)]` code in `offload.rs`.

So nothing removes a registry entry. Every submission `register_submission`
files — `offload.rs:881` (`dispatch_async`'s stream path) and `offload.rs:3647`
(the named-method path) — stays in the map until the process exits.

## What that costs

`register_submission`'s own warning states it:

> gpu offload: {live} submissions are alive and un-finalized. Each one pins host
> writeback buffers, device buffers and a GC-critical token, and the collector
> does not run while any such token is alive — so this grows until the host runs
> out of memory, outside the Java heap and beyond what `-Xmx` bounds.

Finalization is not the leak: `finalize_submission` takes `FinalizeState` out of
its `Mutex<Option<_>>`, so the writeback buffers and the `GcCriticalGuard` are
released on the first `get()` / poll / reaper visit. What leaks is the
`Arc<StreamSubmission>` itself and what it still owns after that — the CUDA
`stream` and `event` handles — plus unbounded growth of the map and of
`live_submission_count()`. The consequence is therefore weaker than the warning
text but not benign: the warning becomes permanently true and stops meaning
"a caller forgot to drain".

## How it happened, and why the removal was right

`b6133b92d` rewrote the synchronous JIT-caller path. It used to
`dispatch_method_from_native` (which registers), then `lookup_submission`,
`finalize_submission`, `release_submission`. It now calls `dispatch_method_sync`,
whose whole point is in its new comment — *"the submission is never registered
and never watched by the reaper"*. With nothing registered there is nothing to
release, so **deleting that call was correct**. It just happened to be the last
one, and no other path had ever released.

## Why it cannot be fixed by putting the call back somewhere

The two obvious release points both change Java-visible behaviour:

* **In the reaper** (`finalize_enqueued_handle`) — the device-done callback runs
  before the Java side has read anything. `GpuFuture.get()` resolves through
  `lookup_submission`, which returns `None` for a released handle, so the future
  would report "unknown handle" for a submission that completed successfully.
* **At the end of `finalize_submission`** — same problem one step later.
  `finalize_submission` is documented as idempotent and
  `gpu_future_synchronize` / the `getNow` path both re-`lookup_submission` per
  call, so a second `get()` on a legally-reusable `Future` would find nothing.

The old synchronous caller avoided exactly this by keeping its own `Arc` alive
across the release, and said so in a comment. That trick does not generalise to
a handle that Java still holds.

## What the fix actually needs

A drain owned by whoever owns the handle's lifetime. Two candidates, neither
wired:

1. **`GpuExecutor.releaseSubmission(long)`** — the API the warning text tells
   users to call, and which `bench-gpu/GpuAsyncChainBench.java` already calls
   (lines 61 and 75). It does not exist: the `craton/gpu/internal/Native`
   registration in `native-builtins/src/craton_gpu.rs` registers
   `openExecutor`, `submit`, `launch`, `submitMethod`, `submitMethodHandle`,
   `submitWithArg`, `submitWithArgs`, `newStream`, `streamSubmitMethod`,
   `futureSynchronize`, `futureGetResult`, `futureGetErrorMessage`,
   `futureGetError` — and no `awaitSubmission` or `releaseSubmission`. There is
   no `GpuExecutor.java` in the tree at all. **So that benchmark cannot run
   today**, which is a second finding.
2. **Executor close** — `offload.rs:1138` says device buffers "are freed when
   the Java `GpuExecutor` is closed", but there is no close path in
   `offload.rs` (`grep` for `close_executor` / `shutdown` / `evict` finds
   nothing). Draining every submission at close is safe by construction — the
   futures are dead — and would bound the leak per executor rather than per
   process.

Either one gives `release_submission` its production caller back and takes
`no_test_only_public_api`'s baseline from 319 to 318 in the same change.

## How it was found

`vm/tests/no_test_only_public_api.rs` — the test-only-public-API ratchet — went
319 against a baseline of 318. Diffing the offender LISTS rather than comparing
the counts (`comm` of the `--nocapture` output at HEAD against the same output
at `1410653b1`, scored with one prebuilt binary over two `vm/src` trees) named
exactly one newcomer and nothing gone:

```
fn release_submission prod=1 test=19 vm/src/runtime/offload.rs
```

`prod=1` is its own declaration. The gate is doing precisely the job its header
describes: a `pub` item whose only callers are its own tests, kept alive from
rustc's `dead_code` lint by those tests, hiding the fact that a subsystem lost
its only cleanup path.

## Reproducer

None runnable here — `gpu-offload` needs CUDA. The static evidence is:

```sh
grep -n "submissions()" vm/src/runtime/offload.rs        # one insert, one remove
grep -rn "release_submission" --include=*.rs .            # callers: tests only
grep -rn "releaseSubmission" --include=*.rs --include=*.java .
cargo test -p cratonvm-vm --test no_test_only_public_api -- --nocapture
```
