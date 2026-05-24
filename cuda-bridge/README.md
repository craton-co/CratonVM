# cuda-bridge

Thin CUDA Driver API bridge for CratonVM GPU offload. **No JVM-specific
code lives here** — only device discovery, module loading, memory
allocation, memcpy, and kernel launch.

## Build modes

| Cargo features  | What you get                                                                 |
| --------------- | ---------------------------------------------------------------------------- |
| _none_          | Crate compiles; every driver-bound entry point returns `DeviceError::NoDriver`. The Phase 2 op-log surfaces (`Stream`, `Event`, `from_host_async`, `to_host_async`, `launch_on_stream`) succeed and record into the in-memory `Vec<StreamOp>` so callers (and tests) can exercise the API without a driver. |
| `cuda`          | Real driver bindings via `cudarc`. Requires CUDA Toolkit 12.x.               |
| `gpu-it`        | Enables `cuda` plus tests that launch real kernels (need an attached GPU).   |

## How to run tests

```
# Stub-backend suite — runs by default on a no-GPU host. Exercises
# the in-memory op-log surface end-to-end (`tests/stub_op_log.rs`),
# the per-module unit tests, and the no-driver contract for the
# synchronous entry points.
cargo test -p cuda-bridge

# Real-driver compile check (no kernels launched). CI runs this so
# `backend_cuda.rs` doesn't silently rot against cudarc API changes.
cargo check -p cuda-bridge --features cuda

# Driver-bound integration tests that launch actual kernels. Require
# an attached NVIDIA GPU and the CUDA toolkit installed.
cargo test -p cuda-bridge --features gpu-it
```

The `cuda` feature compiles `backend_cuda.rs` instead of `backend_stub.rs`;
under that build the op log is empty (the driver owns the queue) and the
stub-only `DeviceContext::stub_for_testing` constructor is not exposed.

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
let out = cuda_bridge::DeviceBuffer::<i32>::zeros(&ctx, 4)?;
let cfg = cuda_bridge::LaunchConfig::elementwise(4);

// Build the argument list with the `KernelArgs` builder: each
// `push_*` call appends one kernel parameter in declaration order.
// `push_device_ptr` retains a keep-alive handle to the buffer's
// device allocation, so the buffers cannot be freed before the
// launch reads them.
let args = cuda_bridge::KernelArgs::new()
    .push_device_ptr(&a)
    .push_device_ptr(&b)
    .push_device_ptr(&out)
    .push_i32(4);
module.launch_raw(&ctx, "vector_add", &cfg, args)?;

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
context's default stream.

**Host-buffer lifetime.** Despite the `_async` suffix, the host-side memcpy
in both `from_host_async` and `to_host_async` is *itself synchronous* under
the hood (the cuda backend uses cudarc's `htod_sync_copy` / `cuCtxSynchronize`
+ `dtoh_sync_copy_into`). The host slice is fully consumed before the call
returns, so `host` / `dst` only need to be borrowed for the duration of the
call — they do **not** need to outlive a subsequent `stream.synchronize()`.
The "async" in the name refers to how the *kernel launches* on `stream` are
ordered against these copies via the dependency events the backend records
internally, not to the host-side memcpy itself.

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

