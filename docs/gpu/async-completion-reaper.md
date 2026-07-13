# Callback-driven async GPU completion

`dispatch_async` completes submissions without requiring Java code to poll.
`cuLaunchHostFunc` signals a process-wide completion reaper, which invokes
`finalize_submission` to drain device-to-host writebacks, release the
GPU-critical guard, and transition the submission to `Completed` or `Failed`.

The writeback attachment happens inside `dispatch_async` before the host
callback is registered. This ordering prevents an immediate callback from
finalizing a submission before its writebacks have been attached.

The reaper is deliberately best-effort for test or embedding VMs that do not
have a `SharedVm::self_arc`: those instances retain the existing poll-based
completion path rather than panicking or hanging.

The public API is documented in [async-api.md](async-api.md). See also [the
historical validation record](../internal/gpu-offload-followups-20260711.md#3-dispatch_async-is-synchronous-under-the-hood--done).
