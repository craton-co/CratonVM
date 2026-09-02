# cuda-bridge

Thin CUDA Driver API bridge for CratonVM GPU offload. **No JVM-specific
code lives here** — only device discovery, module loading, memory
allocation, memcpy, and kernel launch.

## Build modes

| Cargo features  | What you get                                                                 |
| --------------- | ---------------------------------------------------------------------------- |
| _none_          | Crate compiles; every entry point returns `DeviceError::NoDriver`.           |
| `cuda`          | Real driver bindings via `cudarc`. Requires CUDA Toolkit 12.x.               |
| `gpu-it`        | Reserved driver-backed integration alias for `cuda`; no local GPU tests yet. |

## CUDA toolkit version

The `cudarc` dependency is pinned to `cuda-12060`. If your locally
installed driver is on a different CUDA Toolkit major version, update
the feature flag in [`Cargo.toml`](Cargo.toml) and re-run `cargo build
--features cuda`. The cudarc crate gates the FFI bindings by these
feature flags, so a mismatch produces a clear compile-time error rather
than a runtime crash.

## Usage sketch

```ignore
let ctx = cratonvm_cuda_bridge::DeviceContext::new(0)?;
let module = cratonvm_cuda_bridge::DeviceModule::from_ptx(&ctx, PTX, &["vector_add"])?;
let a = cratonvm_cuda_bridge::DeviceBuffer::from_host(&ctx, &[1i32, 2, 3, 4])?;
let b = cratonvm_cuda_bridge::DeviceBuffer::from_host(&ctx, &[10i32, 20, 30, 40])?;
let out = cratonvm_cuda_bridge::DeviceBuffer::<i32>::zeros(&ctx, 4)?;
let cfg = cratonvm_cuda_bridge::LaunchConfig::elementwise(4);

// Build the argument list with the `KernelArgs` builder: each
// `push_*` call appends one kernel parameter in declaration order.
// `push_device_ptr` retains a keep-alive handle to the buffer's
// device allocation, so the buffers cannot be freed before the
// launch reads them.
let args = cratonvm_cuda_bridge::KernelArgs::new()
    .push_device_ptr(&a)
    .push_device_ptr(&b)
    .push_device_ptr(&out)
    .push_i32(4);
// Every launch goes on a caller-created stream; the buffers' own
// `last_write` events order it behind their uploads.
let stream = cratonvm_cuda_bridge::Stream::new(&ctx)?;
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)?;

let mut host = vec![0i32; 4];
out.to_host(&mut host)?;
assert_eq!(host, vec![11, 22, 33, 44]);
```

## Streams, events, and async memcpy

Phase 2 adds the building blocks for overlapping host/device transfers with
kernel execution. The synchronous API in the sketch above is unchanged: every
call still completes before returning. The types in this section are opt-in
and only matter when a caller wants to pipeline work explicitly.

### `Stream`

A `Stream` is a FIFO queue of GPU work bound to a `DeviceContext`. Operations
enqueued on the same stream run in submission order; operations on different
streams may overlap. Construct with `Stream::new(&ctx)`; drop releases the
underlying CUstream. Call `synchronize()` to block the host until every
enqueued op has retired.

```ignore
let ctx = DeviceContext::probe()?;
let stream = Stream::new(&ctx)?;
let buf = DeviceBuffer::<f32>::from_host_async(&ctx, &host[..], &stream)?;
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)?;
let mut out = vec![0.0f32; host.len()];
buf.to_host_async(&mut out, &stream)?; // scoped safe download synchronizes
```

`from_host_async`, `to_host_async`, and `launch_on_stream` order work on the
given stream instead of the context's default stream. The safe memcpy APIs do
not borrow caller-owned host slices across a returned async boundary:

- `from_host_async` copies the source slice into owned staging retained by the
  returned `DeviceBuffer`, then enqueues the upload.
- `to_host_async` downloads into owned staging, synchronizes the stream, and
  copies into the caller's destination before returning.

For fully non-blocking borrowed host buffers, use the unsafe
`from_host_async_unchecked` and `to_host_async_unchecked` variants. Their host
buffers must remain allocated at the same address and not reused until
`stream.synchronize()` or an equivalent event wait proves the DMA has retired.

**Cross-stream ordering.** The bridge installs an upload-completion
event on the buffer when `from_host_async` returns; subsequent
`launch_on_stream` calls that consume the buffer issue
`cuStreamWaitEvent` against that event on the user stream before
launching the kernel, then record a kernel-completion event on the
user stream and stash it as the buffer's new last-write. A subsequent
`to_host_async` waits on that kernel event before issuing the D→H
copy. The H→D / kernel / D→H pipeline thus has no cross-stream races.
The context's own two streams (`copy_h2d`, `copy_d2h`) carry only the
synchronous `from_host` / `to_host` copies, which host-block before
returning.

**Pools.** Freed device allocations return to a per-context pool keyed
by exact size and are reused by the next allocation of that size once
the buffer's last-write event has fired (`CRATONVM_GPU_DEVICE_POOL=0`
disables it). `CRATONVM_GPU_PINNED_H2D=1` routes synchronous uploads
through page-locked staging slabs the context keeps.

### `Event`

An `Event` is a one-shot marker that can be recorded on one stream and waited
on from another. Use it to express cross-stream dependencies without
host-side synchronization.

```ignore
let s1 = Stream::new(&ctx)?;
let s2 = Stream::new(&ctx)?;
let ev = Event::new(&ctx)?;
module.launch_on_stream(&ctx, "stage1", &cfg, args1, &s1)?;
s1.record_event(&ev)?;
s2.wait_event(&ev)?;
module.launch_on_stream(&ctx, "stage2", &cfg, args2, &s2)?;
```

`stage2` will not start before `stage1` has finished, but neither stream
blocks the host. Events are reusable: re-recording on a stream overwrites
the prior marker.

### Stub-mode op log

When the crate is built without the `cuda` feature, every `Stream` records
its enqueued operations into an internal `Vec<StreamOp>` instead of touching
a driver. Tests inspect the log via `stream.ops()` to assert the expected
order of submissions without an attached GPU. In real (`cuda`) mode
`stream.ops()` returns an empty `Vec` — the log only exists in the stub
backend.

```ignore
let ctx = DeviceContext::probe()?;        // stub mode
let stream = Stream::new(&ctx)?;
let buf = DeviceBuffer::<f32>::from_host_async(&ctx, &host, &stream)?;
module.launch_on_stream(&ctx, "k", &cfg, args, &stream)?;
buf.to_host_async(&mut out, &stream)?;
stream.synchronize()?;
assert_eq!(stream.ops().len(), 4);        // 3 enqueues + 1 sync
```

### `StreamOp` variants

The op log captures one variant per enqueue or synchronization call:

| Variant         | Captures                                                       |
| --------------- | -------------------------------------------------------------- |
| `UploadAsync`   | byte count of `from_host_async` (H→D) copies.                  |
| `DownloadAsync` | byte count of `to_host_async` (D→H) copies.                    |
| `Launch`        | kernel name, `grid` and `block` dims passed to `launch_on_stream`. |
| `EventRecord`   | event id recorded into the stream.                             |
| `EventWait`     | event id the stream is gated on.                               |
| `Synchronize`   | a host-side `stream.synchronize()` call.                       |

## License

Apache-2.0. See `../LICENSE` and `../NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.

