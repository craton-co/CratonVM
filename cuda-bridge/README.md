# cuda-bridge

Thin CUDA Driver API bridge for RustJVM GPU offload. **No JVM-specific
code lives here** — only device discovery, module loading, memory
allocation, memcpy, and kernel launch.

## Build modes

| Cargo features  | What you get                                                                 |
| --------------- | ---------------------------------------------------------------------------- |
| _none_          | Crate compiles; every entry point returns `DeviceError::NoDriver`.           |
| `cuda`          | Real driver bindings via `cudarc`. Requires CUDA Toolkit 12.x.               |
| `gpu-it`        | Enables `cuda` plus tests that launch real kernels (need an attached GPU).   |

## CUDA toolkit version

The `cudarc` dependency is pinned to `cuda-12060`. If your locally
installed driver is on a different CUDA Toolkit major version, update
the feature flag in [`Cargo.toml`](Cargo.toml) and re-run `cargo build
--features cuda`. The cudarc crate gates the FFI bindings by these
feature flags, so a mismatch produces a clear compile-time error rather
than a runtime crash.

## Usage sketch

```ignore
let ctx = cuda_bridge::DeviceContext::new(0)?;
let module = cuda_bridge::DeviceModule::from_ptx(&ctx, PTX, &["vector_add"])?;
let a = cuda_bridge::DeviceBuffer::from_host(&ctx, &[1i32, 2, 3, 4])?;
let b = cuda_bridge::DeviceBuffer::from_host(&ctx, &[10i32, 20, 30, 40])?;
let mut out = cuda_bridge::DeviceBuffer::<i32>::zeros(&ctx, 4)?;
let cfg = cuda_bridge::LaunchConfig::elementwise(4);
module.launch(&ctx, "vector_add", &cfg, (&a, &b, &mut out, 4i32))?;
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
buf.to_host_async(&mut out, &stream)?;
stream.synchronize()?;          // block until all three steps done
```

`from_host_async`, `to_host_async`, and `launch_on_stream` mirror the
synchronous variants but enqueue work onto the given stream instead of the
context's default stream. The host-side slices passed to the async memcpy
helpers must outlive the stream synchronization point.

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
| `HtoDAsync`     | byte count and element type tag for `from_host_async` copies.  |
| `DtoHAsync`     | byte count and element type tag for `to_host_async` copies.    |
| `Launch`        | kernel name and `LaunchConfig` passed to `launch_on_stream`.   |
| `RecordEvent`   | event id recorded into the stream.                             |
| `WaitEvent`     | event id the stream is gated on.                               |
| `Synchronize`   | a host-side `stream.synchronize()` call.                       |

