# Streams and events — async pipeline reference

Phase 2 user guide for the `cuda-bridge` async stack. Covers `Stream`,
`Event`, async memcpy, `launch_on_stream`, and the stub-mode op log used
for tests on CPU-only machines.

This document is the **user guide**. For crate-level layout and FFI
details see [`cuda-bridge/README.md`](../../cuda-bridge/README.md). For
build-mode and CLI flags see [`README.md`](README.md) (everything here
inherits the `gpu` / `gpu-driver` feature gating unchanged).

> **Phase 2 is Rust-side only.** The async API is consumed by
> `vm::runtime::offload` and `vm::runtime::gpu_marshal`. No Java code,
> annotation, or CLI flag changes. Java-facing async surface is Phase 3.

## Quick start

**Before — synchronous pipeline.** Every memcpy and launch blocks until
done. Useful but leaves the device idle between stages.

```rust,ignore
let ctx = DeviceContext::new(0)?;
let module = DeviceModule::from_ptx(&ctx, PTX, &["vector_add"])?;

let a = DeviceBuffer::from_host(&ctx, &host_a)?;     // blocks
let b = DeviceBuffer::from_host(&ctx, &host_b)?;     // blocks
let mut out = DeviceBuffer::<i32>::zeros(&ctx, n)?;
let cfg = LaunchConfig::elementwise(n);
module.launch(&ctx, "vector_add", &cfg, (&a, &b, &mut out, n as i32))?;
out.to_host(&mut host_out)?;                          // blocks
```

**After — async pipeline.** Uploads, the kernel, and the download are
all queued on one stream and overlap with host work until
`synchronize()`.

```rust,ignore
let ctx = DeviceContext::new(0)?;
let module = DeviceModule::from_ptx(&ctx, PTX, &["vector_add"])?;
let stream = Stream::new(&ctx)?;

let a = DeviceBuffer::from_host_async(&ctx, &stream, &host_a)?;
let b = DeviceBuffer::from_host_async(&ctx, &stream, &host_b)?;
let mut out = DeviceBuffer::<i32>::zeros(&ctx, n)?;
let cfg = LaunchConfig::elementwise(n);
module.launch_on_stream(&ctx, &stream, "vector_add", &cfg,
                        (&a, &b, &mut out, n as i32))?;
out.to_host_async(&stream, &mut host_out)?;

// ... do other host work here, queued GPU work runs in parallel ...

stream.synchronize()?;   // host blocks until the queue drains
```

## `Stream`

A `Stream` is an ordered queue of device operations. Operations within
one stream run **FIFO**; operations across different streams are
**concurrent** (subject to the device's resources).

### Construction

```rust,ignore
let stream = Stream::new(&ctx)?;
```

A stream is bound to its context. Dropping the stream synchronizes it
first, then releases the underlying `CUstream`. Dropping is safe even if
work is still in flight — the destructor waits.

### Op flow

Each async entry point (`from_host_async`, `to_host_async`,
`launch_on_stream`, `Event::record`, `stream.wait_for`) appends one
operation to the stream. The driver pulls them off in order and issues
them to the GPU.

```rust,ignore
let buf = DeviceBuffer::from_host_async(&ctx, &stream, &host)?;   // op 1: H2D
module.launch_on_stream(&ctx, &stream, "k", &cfg, args)?;          // op 2: launch
buf.to_host_async(&stream, &mut out)?;                              // op 3: D2H
stream.synchronize()?;                                              // host wait
```

After `synchronize()` returns, every op enqueued before that call has
completed and host-visible memory is up-to-date. Re-using `stream` after
`synchronize()` is fine; the queue is simply empty again.

### `synchronize()`

```rust,ignore
stream.synchronize()?;
```

Blocks the calling host thread until the stream's queue drains. Cheap if
the queue is already empty. Returns `DeviceError::StreamSync` on driver
error.

## `Event`

An `Event` is a one-shot marker that can be **recorded** on one stream
and **waited on** from another stream. Use events to express
dependencies between streams without involving the host.

### Recording and waiting

```rust,ignore
let upload = Stream::new(&ctx)?;
let compute = Stream::new(&ctx)?;

let buf = DeviceBuffer::from_host_async(&ctx, &upload, &host)?;
let after_upload = Event::record(&upload)?;

// `compute` will not start until `upload` reaches `after_upload`:
compute.wait_for(&after_upload)?;
module.launch_on_stream(&ctx, &compute, "k", &cfg, args)?;
compute.synchronize()?;
```

`Event::record(&stream)` enqueues a marker on `stream` and returns the
event. `stream.wait_for(&event)` makes `stream` block on the GPU side
(not the host) until that marker is reached. The host stays unblocked
the whole time.

### Cross-stream dependency graph: three streams

A common shape: upload, compute, download — overlapped, but ordered
correctly.

```rust,ignore
let s_h2d  = Stream::new(&ctx)?;   // host → device uploads
let s_comp = Stream::new(&ctx)?;   // kernels
let s_d2h  = Stream::new(&ctx)?;   // device → host downloads

// 1. Upload on s_h2d.
let a = DeviceBuffer::from_host_async(&ctx, &s_h2d, &host_a)?;
let b = DeviceBuffer::from_host_async(&ctx, &s_h2d, &host_b)?;
let e_uploaded = Event::record(&s_h2d)?;

// 2. Compute on s_comp, but only after uploads finish.
s_comp.wait_for(&e_uploaded)?;
let mut out = DeviceBuffer::<i32>::zeros(&ctx, n)?;
module.launch_on_stream(&ctx, &s_comp, "vector_add", &cfg,
                        (&a, &b, &mut out, n as i32))?;
let e_computed = Event::record(&s_comp)?;

// 3. Download on s_d2h, but only after compute finishes.
s_d2h.wait_for(&e_computed)?;
out.to_host_async(&s_d2h, &mut host_out)?;

// Host waits only for the final download.
s_d2h.synchronize()?;
```

The host issues every op immediately and never blocks until the last
`synchronize()`. The GPU sees a correct dependency chain.

### What events do **not** provide

- **No timing.** Phase 2 events are markers, not timers. There is no
  `event.elapsed_ms(&other)`. Use external profiling for timing.
- **No host-side polling.** There is no `event.is_complete()`. If the
  host needs to know, call `stream.synchronize()` on a stream that
  `wait_for`'d the event.

## Async memcpy

Two new entry points on `DeviceBuffer`. Both take a `&Stream` and return
immediately after queueing the copy.

```rust,ignore
let buf = DeviceBuffer::from_host_async(&ctx, &stream, &host)?;
// ... queue more work ...
buf.to_host_async(&stream, &mut out)?;
stream.synchronize()?;
```

### Ownership rules

These are the rules that protect you from race conditions:

| Direction | Rule |
| --- | --- |
| `from_host_async` (H2D) | The host slice **must remain valid and unchanged** until the next `synchronize()` (or an event the caller waits on) confirms the copy completed. |
| `to_host_async` (D2H) | The host slice **must not be read** until the stream is synchronized. The bytes are undefined until then. |

Concretely: do not free the host buffer, do not mutate it, and do not
read the destination buffer until the stream that owns the copy is
synchronized. The borrow checker enforces the lifetime; it does **not**
enforce the "don't read until synced" half — that is a contract.

### When to use sync vs async

| Use sync (`from_host`, `to_host`) when… | Use async (`*_async`) when… |
| --- | --- |
| The transfer is the only GPU operation you have in flight. | You can overlap it with other GPU work or other host work. |
| You need the data on the next host line. | You can defer the read until later in the function. |
| You are writing a one-off test or probe. | You are building a pipeline (`offload.rs` dispatch). |

Reaching for `from_host` inside a pipelined `try_dispatch` defeats the
purpose. Reaching for `from_host_async` for a single 16-byte
configuration upload is unnecessary ceremony.

## `launch_on_stream`

Non-blocking kernel launch. Same signature as `launch_raw` plus a
`&Stream`.

```rust,ignore
module.launch_on_stream(&ctx, &stream, "vector_add", &cfg, args)?;
```

The call returns once the launch is queued, not once the kernel
completes. To wait for completion, either:

- `stream.synchronize()?;` — host blocks for the entire queue, **or**
- `let done = Event::record(&stream)?;` followed by
  `other_stream.wait_for(&done)?;` — GPU-side dependency on another
  stream, host stays unblocked.

Using `launch_raw` on a default stream and `launch_on_stream` on the
same context interleaves through the default stream's serialization
rules; prefer to commit to one model per dispatch path.

## Stub-mode op log

When `cuda-bridge` is built **without** the `cuda` feature (the default
on machines with no driver), every `Stream` method is a no-op except for
one thing: it appends a record to an internal `Vec<StreamOp>` that you
can introspect from tests.

```rust,ignore
use cuda_bridge::{Stream, StreamOp, DeviceBuffer};

let ctx = DeviceContext::new(0)?;          // stub context
let stream = Stream::new(&ctx)?;

let a = DeviceBuffer::from_host_async(&ctx, &stream, &[1i32, 2, 3])?;
module.launch_on_stream(&ctx, &stream, "k", &cfg, args)?;
a.to_host_async(&stream, &mut out)?;

let ops = stream.ops();
assert!(matches!(ops[0], StreamOp::MemcpyH2DAsync { len: 3, .. }));
assert!(matches!(ops[1], StreamOp::Launch { ref name, .. } if name == "k"));
assert!(matches!(ops[2], StreamOp::MemcpyD2HAsync { len: 3, .. }));
```

`Stream::ops() -> Vec<StreamOp>` returns a snapshot. Repeated calls
return the cumulative log; `Stream::clear_ops()` resets it. The log is
only populated under the stub backend — under `cuda` it is always empty
(zero overhead in release builds).

This is the **primary way** to test offload pipelines without a GPU.
A typical assertion sequence:

```rust,ignore
#[test]
fn pipeline_uploads_before_launch() {
    let ctx = DeviceContext::new(0).unwrap();
    let stream = Stream::new(&ctx).unwrap();
    run_my_pipeline(&ctx, &stream);

    let ops = stream.ops();
    let upload_idx = ops.iter().position(|o| matches!(o,
        StreamOp::MemcpyH2DAsync { .. })).unwrap();
    let launch_idx = ops.iter().position(|o| matches!(o,
        StreamOp::Launch { .. })).unwrap();
    assert!(upload_idx < launch_idx, "uploads must precede launch");
}
```

### `StreamOp` variants

| Variant | Recorded by | Carries |
| --- | --- | --- |
| `MemcpyH2DAsync { dst_ptr, len, elem_size }` | `DeviceBuffer::from_host_async` | Destination device pointer, element count, element size in bytes. |
| `MemcpyD2HAsync { src_ptr, len, elem_size }` | `DeviceBuffer::to_host_async` | Source device pointer, element count, element size in bytes. |
| `Launch { name, grid, block, shared_bytes }` | `DeviceModule::launch_on_stream` | Kernel name (owned `String`), grid dims, block dims, dynamic shared-memory bytes. |
| `EventRecord { event_id }` | `Event::record` | Stable per-event id usable to correlate with `WaitForEvent`. |
| `WaitForEvent { event_id }` | `Stream::wait_for` | The id of the event being waited on. |
| `Synchronize` | `Stream::synchronize` | (no payload) |

`event_id` is a monotonic counter assigned at `Event::record`; tests can
correlate "this `WaitForEvent` matches that `EventRecord`" without
having to thread the actual `Event` through.

## What this is **not**

- **Not a Java-visible API.** Phase 2 surfaces async only inside Rust.
  No new bytecode hooks, no new CLI flags, no annotation. Phase 3 will
  decide whether to expose anything to Java code.
- **Not graph capture.** There is no `Stream::begin_capture` / `Graph`
  type. Phase 2 is plain stream/event semantics. Graph capture, if it
  lands, is a separate proposal.
- **Not event timing.** Events are dependency markers only. There is no
  `elapsed_ms` between events; use external profiling tools.
- **Not a new feature flag.** Streams and events ride the existing
  `gpu` / `gpu-driver` feature flags described in
  [`README.md`](README.md). A CPU-only build still sees nothing.

## Limitations

- **No priority streams.** All streams are created with the default
  priority. The CUDA driver supports priorities; we have not exposed
  them. Add `Stream::with_priority(...)` when a workload demands it.
- **Stream count is not bounded by `cuda-bridge`.** Each `Stream` is
  one `CUstream`; CUDA imposes its own limits per context. Creating
  thousands of streams will work until the driver complains. The
  expected count for offload dispatch is small (1–3 per active
  kernel).
- **Events are single-shot semantically.** Recording the same `Event`
  twice on different streams is undefined — `Event::record` returns a
  new `Event` each call. Treat events as values, not handles to reuse.
- **No host callbacks.** There is no `stream.add_host_callback(...)`
  hook. To run host code after a stream completes, call
  `stream.synchronize()?;` and run it inline.
- **Default-stream interaction.** `launch_raw` issues on the default
  CUDA stream; mixing it with `launch_on_stream` in the same dispatch
  serializes through CUDA's legacy default-stream semantics. Pick one
  style per code path.
- **No multi-device awareness.** `Stream` is bound to a single
  `DeviceContext`. Multi-GPU pipelines need one stream per context;
  events do not cross contexts.

## Best practices

- **One stream per logical pipeline.** Don't share a stream between
  unrelated offload calls — you lose the concurrency benefit.
- **Pin host buffers when you can.** Async memcpy is fastest with
  pinned (page-locked) memory. `cuda-bridge` does not yet expose a
  pinning API; until then expect bandwidth lower than peak.
- **Synchronize at the boundary, not after every op.** The whole point
  of the async API is that you queue several ops and call
  `synchronize()` once.
- **Use the stub op log in CI.** Every offload pipeline test should
  assert on `stream.ops()` rather than mocking the bridge. The log is
  the contract.

## See also

- [`README.md`](README.md) — top-level GPU offload reference, build
  modes, CLI flags.
- [`cuda-bridge/README.md`](../../cuda-bridge/README.md) — crate-level
  layout, FFI surface, feature flags. The Phase 2 section there shows
  the same examples from the crate's point of view.
- [`plan.md`](plan.md) — execution plan and per-part status.
