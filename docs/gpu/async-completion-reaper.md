# Async GPU completion: the reaper polls

`dispatch_async` completes submissions without requiring Java code to poll.
A process-wide completion reaper thread watches every submission handed to
it and, when the device reports the submission's completion event fired,
invokes `finalize_submission` to drain device-to-host writebacks, release
the GPU critical-section token, and transition the submission to
`Completed` or `Failed`.

## How the reaper learns a kernel finished

**Since 2026-09-02: by asking.** The reaper polls each watched submission
with `cuEventQuery` (`Event::query`), yielding between passes and sleeping
up to 200 µs once nothing has finished for a few passes. A query asks the
device a question and enqueues nothing.

**Before: by being told.** Every dispatch registered a `cuLaunchHostFunc`
callback that flagged the submission and woke the reaper. A host function
on a stream runs after the work ahead of it **and blocks every launch
enqueued behind it on that stream until it returns**, so a chain of kernels
on one stream paid a driver-thread round trip between every pair.
GPULlama3's 453-launch decode step measured 14 ms of host time against
24 ms of device time, and the graph-capture path — which skips the callback
— measured 2.05x. `CRATONVM_GPU_HOST_CALLBACK=1` restores the callback as
the same-binary A/B lever.

## What does not go through the reaper

The transparent interpreter hook (`try_dispatch`) blocks on the completion
event before it returns, so its submission is never registered, never
watched, and never had a callback: it is finalized by the caller
(`Completion::Caller`). The explicit async API (`GpuFuture`) uses the
reaper (`Completion::Reaper`).

The writeback attachment happens inside `dispatch_async` before the
submission is handed to the reaper. This ordering prevents an immediate
completion from finalizing a submission before its writebacks have been
attached.

The reaper is deliberately best-effort for test or embedding VMs that do
not have a `SharedVm::self_arc`: those instances retain the poll-based
completion path (`futureIsDone` / `get()`) rather than panicking or
hanging.

The public API is documented in [async-api.md](async-api.md).
