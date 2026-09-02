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
let args = KernelArgs::new()
    .push_device_ptr(&a)
    .push_device_ptr(&b)
    .push_device_ptr(&out)
    .push_i32(n as i32);
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)?;
out.to_host(&mut host_out)?;                          // blocks
```

**After — async pipeline.** Uploads and the kernel launch are queued on
one stream and return without blocking, so host work between them
overlaps with the device. (The download step is a separate story — see
the note after the snippet.)

```rust,ignore
let ctx = DeviceContext::new(0)?;
let module = DeviceModule::from_ptx(&ctx, PTX, &["vector_add"])?;
let stream = Stream::new(&ctx)?;

let a = DeviceBuffer::from_host_async(&ctx, &host_a, &stream)?;
let b = DeviceBuffer::from_host_async(&ctx, &host_b, &stream)?;
let mut out = DeviceBuffer::<i32>::zeros(&ctx, n)?;
let cfg = LaunchConfig::elementwise(n);
let args = KernelArgs::new()
    .push_device_ptr(&a)
    .push_device_ptr(&b)
    .push_device_ptr(&out)
    .push_i32(n as i32);
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)?;

// ... do other host work here; the two uploads and the launch above
// were queued without blocking, so they run in parallel with it ...

out.to_host_async(&mut host_out, &stream)?;
```

> **`to_host_async` (the safe wrapper) is not fire-and-forget.** Unlike
> `from_host_async` and `launch_on_stream`, the safe `to_host_async`
> downloads into an owned staging buffer and then calls
> `stream.synchronize()` **internally** before returning — see the
> `# Current safety contract` doc comment on `DeviceBuffer::to_host_async`
> in `cuda-bridge/src/lib.rs`. So by the time the call above returns,
> `stream` has already drained; there is no additional host-work window
> after it, and an explicit trailing `stream.synchronize()` is
> redundant (harmless — synchronizing an empty queue is cheap — but
> redundant). The genuinely non-blocking download is
> `to_host_async_unchecked`, which hands you the borrowed-destination
> contract directly instead of synchronizing on your behalf.

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
`launch_on_stream`, `Stream::record_event`, `Stream::wait_event`)
appends one operation to the stream. The driver pulls them off in
order and issues them to the GPU.

```rust,ignore
let buf = DeviceBuffer::from_host_async(&ctx, &host, &stream)?;   // op 1: H2D
module.launch_on_stream(&ctx, "k", &cfg, args, &stream)?;          // op 2: launch
buf.to_host_async(&mut out, &stream)?;                              // op 3: D2H
stream.synchronize()?;                                              // host wait (redundant here — see the note above; to_host_async already synced)
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

There is no single call that both allocates and records an event.
`Event::new(&ctx)` creates it; `stream.record_event(&event)` enqueues
it on a stream's queue; `stream.wait_event(&event)` makes a (possibly
different) stream block on the GPU side until that marker is reached:

```rust,ignore
let upload = Stream::new(&ctx)?;
let compute = Stream::new(&ctx)?;

let buf = DeviceBuffer::from_host_async(&ctx, &host, &upload)?;
let after_upload = Event::new(&ctx)?;
upload.record_event(&after_upload)?;

// `compute` will not start until `upload` reaches `after_upload`:
compute.wait_event(&after_upload)?;
module.launch_on_stream(&ctx, "k", &cfg, args, &compute)?;
compute.synchronize()?;
```

`stream.record_event(&event)` enqueues `event` at the current point in
`stream`'s queue. `other_stream.wait_event(&event)` makes
`other_stream` block on the GPU side (not the host) until that marker
is reached; the host stays unblocked the whole time.

In the snippet above the manual `record_event`/`wait_event` pair is
actually redundant: `DeviceBuffer::from_host_async` already allocates
its own completion event and stamps it into the buffer's `last_write`
slot internally, and `DeviceModule::launch_on_stream` already waits on
every device-pointer argument's `last_write` event before launching —
see [Automatic per-buffer ordering](#automatic-per-buffer-ordering-last_write-events)
below. Reach for `Event::new` / `record_event` / `wait_event` directly
when you need a happens-before relation that isn't already carried by
a shared buffer (e.g. gating one stream's kernel on a completely
separate stream's unrelated kernel).

### Two things `Event` does that the API does not show

Both landed 2026-08-29, both are invisible to callers, and both exist
because a kernel submission is a per-launch cost multiplied by hundreds:
GPULlama3's forward pass makes 453 of them per token, and each one mints
two events — one inside `launch_on_stream` for the `last_write` marker,
one in `vm::runtime::offload` for the submission-completion marker.

**`Event::new` recycles.** `DeviceContext` keeps a capped free list of
`CUevent` handles, and `Event::drop` returns its handle to that list
instead of destroying it. ~900 `cuEventCreate` / `cuEventDestroy` pairs a
token become ~900 `Vec` pops. This is sound because `cuStreamWaitEvent`
captures an event's contents **at the time of the call**: a wait already
issued against a handle cannot be reached back into and satisfied by a
later re-record of that same handle. A handle only ever returns to the
pool when the last `Arc<Event>` holding it is gone.

**`Stream::wait_event` skips a wait on its own stream.** An event recorded
on stream `S` needs no `cuStreamWaitEvent(S, …)`: everything queued on a
stream before a point is already ordered before everything queued after
it. `Event` remembers which raw stream last recorded it, and
`Stream::wait_event` skips that case. It is not a rare one:
it is every launch in a single-stream chain, and every launch after the
first in a chunked dispatch, each of which would otherwise wait on the
`last_write` its predecessor stamped on the same stream.

`cratonvm_types::gpu_event_census` counts created vs recycled and waits
issued vs elided, printed on the exit path under
`CRATONVM_GPU_TIME_DISPATCH=1`. A change that never fires and a change
that fires without helping look identical from a wall clock; the census is
what tells them apart.

### Cross-stream dependency graph: three streams

A common shape: upload, compute, download — overlapped, but ordered
correctly.

```rust,ignore
let s_h2d  = Stream::new(&ctx)?;   // host → device uploads
let s_comp = Stream::new(&ctx)?;   // kernels
let s_d2h  = Stream::new(&ctx)?;   // device → host downloads

// 1. Upload on s_h2d.
let a = DeviceBuffer::from_host_async(&ctx, &host_a, &s_h2d)?;
let b = DeviceBuffer::from_host_async(&ctx, &host_b, &s_h2d)?;

// 2. Compute on s_comp. No manual wait needed here — see the note
//    below: `launch_on_stream` waits on each input buffer's own
//    last_write event automatically.
let mut out = DeviceBuffer::<i32>::zeros(&ctx, n)?;
let args = KernelArgs::new()
    .push_device_ptr(&a)
    .push_device_ptr(&b)
    .push_device_ptr(&out)
    .push_i32(n as i32);
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &s_comp)?;

// 3. Download on s_d2h. Also no manual wait needed — to_host_async
//    (safe) synchronizes its own stream internally, ordered behind
//    `out`'s last_write (the kernel above) automatically.
out.to_host_async(&mut host_out, &s_d2h)?;
```

The host issues the uploads and the launch without blocking. The GPU
sees a correct H2D → compute dependency chain even though this
snippet never calls `Event::new` / `record_event` / `wait_event`
directly, because both steps are buffer-driven — see the next
section. (The manual `Event::record`-and-`wait_for`-style choreography
shown further up this document, in an earlier revision, described a
step that `launch_on_stream` now performs for you automatically for
any argument passed via `push_device_ptr`.)

### Automatic per-buffer ordering (`last_write` events)

You do not have to hand-roll the H2D → compute → D2H event chain
above for buffers that flow through `push_device_ptr`.
`DeviceModule::launch_on_stream` (`cuda-bridge/src/launch.rs`) owns a
"cross-stream ordering choreography" keyed on each `DeviceBuffer`'s
own `last_write` slot:

1. Before launching, for every `KernelArg::DevicePtr` argument whose
   buffer has a `last_write` event set (populated by a prior
   `from_host_async` upload or a prior `launch_on_stream` that wrote
   it), the launch stream calls `wait_event` on it — gating the new
   kernel behind whatever last touched that buffer, even if that prior
   write happened on a *different* stream.
2. After the launch is queued, a fresh `kernel_done` event is recorded
   on the launch stream and stamped into `last_write` for every
   device-pointer argument, so the *next* consumer (another
   `launch_on_stream`, or a `to_host_async` D2H) automatically orders
   behind this kernel too.

`DeviceBuffer::to_host`/`to_host_async` participate in the same
scheme on the read side: they wait on the buffer's own `last_write`
event (not a context-wide event) before downloading, so a D2H copy is
correctly ordered behind whichever kernel or upload actually produced
that buffer's current contents — even under concurrent pipelines
touching unrelated buffers on other streams.

Net effect: as long as you thread the same `DeviceBuffer`s through
`from_host_async` → `launch_on_stream` → `to_host_async`/`to_host`,
the streams involved can be freely mixed (one stream per stage, one
stream for everything, whatever) and the ordering above is enforced
without any manual `Event`/`wait_event` calls. Reach for the manual
`Event` API from the previous section only for dependencies that
aren't mediated by a shared buffer.

### What events do **not** provide

- **No timing.** Phase 2 events are markers, not timers. There is no
  `event.elapsed_ms(&other)`. Use external profiling for timing.
- **Host-side polling exists but isn't used by the offload dispatch
  layer yet.** `Event::query() -> Result<bool>` *is* implemented (both
  backends — `cuda-bridge/src/event.rs`): in `cuda` mode it wraps
  `cuEventQuery`, mapping `CUDA_ERROR_NOT_READY` to `Ok(false)`; in
  stub mode it returns whether the event has been recorded on some
  stream. It is a genuine non-blocking probe. What's still missing is
  a consumer: `vm::runtime::offload`'s `GpuFuture` completion path
  (`finalize_submission`) only ever calls the *blocking*
  `event.synchronize()`, never `query()` — see
  [`async-api.md`'s Current limitations](async-api.md#current-limitations).
  Wiring `query()` into a poll- or callback-driven completion path is
  in-progress work, not shipped.

## Async memcpy

Two new entry points on `DeviceBuffer`. Both take a `&Stream` and return
immediately after queueing the copy.

```rust,ignore
let buf = DeviceBuffer::from_host_async(&ctx, &host, &stream)?;
// ... queue more work ...
buf.to_host_async(&mut out, &stream)?;
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

> **This table describes the borrowed contract, which the safe
> wrappers mostly enforce for you rather than hand you.** `DeviceBuffer::
> from_host_async` (safe) copies the caller's slice into an
> owned `Arc<Vec<T>>` staging buffer before queuing the upload, so the
> caller's original slice does **not** need to stay valid past the
> call — the borrow-and-don't-mutate rule above literally applies to
> `from_host_async_unchecked`, the borrowed variant. Symmetrically,
> the safe `to_host_async` downloads into owned staging memory and
> then calls `stream.synchronize()` **before returning**, so by the
> time it hands you `dst` the copy has already completed — you cannot
> observe undefined bytes through it. The borrowed-and-don't-read-early
> contract applies to `to_host_async_unchecked`. Use the `_unchecked`
> variants only when you specifically want to avoid the extra copy /
> the internal synchronize and are prepared to uphold the lifetime
> contract yourself.

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

Non-blocking kernel launch, and since 2026-09-02 the ONLY kernel launch
the bridge offers:

```rust,ignore
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)?;
```

The call returns once the launch is queued, not once the kernel
completes. To wait for completion, either:

- `stream.synchronize()?;` — host blocks for the entire queue, **or**
- `let done = Event::new(&ctx)?; stream.record_event(&done)?;`
  followed by `other_stream.wait_event(&done)?;` — GPU-side dependency
  on another stream, host stays unblocked. In practice you rarely need
  to do this by hand for buffer-mediated dependencies —
  `launch_on_stream` already records and stamps a `kernel_done` event
  per output buffer; see [Automatic per-buffer
  ordering](#automatic-per-buffer-ordering-last_write-events) above.

### Under the hood: what the context's own streams are for

`DeviceContext` owns two forked streams, `copy_h2d` and `copy_d2h`, and
they serve only the SYNCHRONOUS copies: `DeviceBuffer::from_host` and
`copy_from_host` upload on the first and host-block on it before
returning; `to_host` downloads on the second, after a
`cuStreamWaitEvent` on the buffer's own `last_write`, and host-blocks on
it. Neither is CUDA's legacy default stream (stream `0`).

Until 2026-09-02 the context also owned a `compute` stream and two
context-wide barrier events, and a `DeviceModule::launch_raw` submitted
onto that stream. Nothing in the VM called it, but every synchronous
upload still recorded one of the barrier events for it. Both are gone:
every kernel goes through `launch_on_stream` on a caller-created
`Stream`, ordered by the per-buffer `last_write` events, and the
synchronous copies record nothing.

The context also owns two allocation pools (`AllocPool` for device
memory, `PinnedPool` for page-locked upload staging) — see
[`docs/gpu/README.md`](README.md) for their kill switches.

## Stub-mode op log

When `cuda-bridge` is built **without** the `cuda` feature (the default
on machines with no driver), every `Stream` method is a no-op except for
one thing: it appends a record to an internal `Vec<StreamOp>` that you
can introspect from tests.

```rust,ignore
use cuda_bridge::{Stream, StreamOp, DeviceBuffer};

let ctx = DeviceContext::new(0)?;          // stub context
let stream = Stream::new(&ctx)?;

let a = DeviceBuffer::from_host_async(&ctx, &[1i32, 2, 3], &stream)?;
module.launch_on_stream(&ctx, "k", &cfg, args, &stream)?;
a.to_host_async(&mut out, &stream)?;

let ops = stream.ops();
let upload_pos = ops.iter().position(
    |op| matches!(op, StreamOp::UploadAsync { bytes: 12 })   // 3 × 4-byte i32
).expect("upload recorded");
let launch_pos = ops.iter().position(
    |op| matches!(op, StreamOp::Launch { ref kernel, .. } if kernel == "k")
).expect("launch recorded");
let download_pos = ops.iter().position(
    |op| matches!(op, StreamOp::DownloadAsync { bytes: 12 })
).expect("download recorded");
assert!(upload_pos < launch_pos && launch_pos < download_pos);
```

`Stream::ops() -> Vec<StreamOp>` returns a snapshot (a clone of the
internal log, not a drain). Repeated calls return the cumulative log —
**there is no `Stream::clear_ops()`**; the log only ever grows for the
lifetime of the `Stream`, so tests that need a clean log should
construct a fresh `Stream` rather than try to reset an existing one.
The log is only populated under the stub backend — under `cuda` it is
always empty (zero overhead in release builds).

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
        StreamOp::UploadAsync { .. })).unwrap();
    let launch_idx = ops.iter().position(|o| matches!(o,
        StreamOp::Launch { .. })).unwrap();
    assert!(upload_idx < launch_idx, "uploads must precede launch");
}
```

### `StreamOp` variants

| Variant | Recorded by | Carries |
| --- | --- | --- |
| `UploadAsync { bytes }` | `DeviceBuffer::from_host_async` | Total byte count of the upload. No destination-pointer or per-element breakdown. |
| `DownloadAsync { bytes }` | `DeviceBuffer::to_host_async` | Total byte count of the download. |
| `Launch { kernel, grid, block }` | `DeviceModule::launch_on_stream` | Kernel name (owned `String`), grid dims, block dims. There is no `shared_bytes` field — dynamic shared-memory size is not captured in the op log. |
| `EventRecord { event_id }` | `Stream::record_event` | Stable per-event id usable to correlate with `EventWait`. |
| `EventWait { event_id }` | `Stream::wait_event` | The id of the event being waited on. |
| `Synchronize` | `Stream::synchronize` | (no payload) |

`event_id` is a monotonic counter assigned at `Event::new` (not at
recording time — an event can be constructed and never recorded);
tests can correlate "this `EventWait` matches that `EventRecord`"
without having to thread the actual `Event` through.

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
- **Events are single-shot semantically.** Calling `record_event` on
  the same `Event` from two different streams is undefined — each
  `Event::new(&ctx)` is meant to back exactly one `record_event` call.
  Treat events as values produced fresh per synchronization point, not
  handles to re-record.
- **Host callbacks block the stream behind them.**
  `Stream::add_host_callback` exists (`cuLaunchHostFunc`), and its
  contract is the CUDA one: the function runs after the work ahead of it
  and every launch enqueued after it on that stream waits until it
  returns. That is a host round trip between kernels, which is why the
  VM's completion reaper stopped registering one per launch on
  2026-09-02 and polls `Event::query` instead
  (`CRATONVM_GPU_HOST_CALLBACK=1` restores the callback). Use it for a
  notification, never per launch in a chain.
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
  layout, FFI surface, feature flags.
