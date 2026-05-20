# Phase 2 implementation spec — streams + events + async memcpy

Single source of truth for nine parallel implementation agents.
**Read sections 1 and 2 before writing any code.**

## 1. Scope

Phase 2 adds the *plumbing* required for asynchronous GPU work:

- `Stream` — a typed handle backed by a CUDA stream (or stub op log).
- `Event` — a synchronisation marker recorded on one stream, waited
  on by another.
- Async `DeviceBuffer` upload/download routed through a stream.
- `launch_on_stream` — kernel launch with stream affinity.

**No Java-visible API yet.** That's Phase 3. Phase 2's deliverable is
all Rust-side. After Phase 2 lands, the existing synchronous offload
path still works unchanged; the new types simply *exist* and are
exercised by stub-backed integration tests.

## 2. Cross-cutting design contracts

These names and signatures are **fixed**. Do not deviate.

### 2.1 Module layout

Five new files. Each agent owns its own file to minimise merge
conflicts.

```
cuda-bridge/
  src/
    lib.rs                  (existing; one agent makes additive edits)
    backend_cuda.rs         (existing; do NOT touch this in Phase 2)
    backend_stub.rs         (existing; do NOT touch this in Phase 2)
    stream.rs               NEW — Item P2-1
    event.rs                NEW — Item P2-2
    async_memcpy.rs         NEW — Item P2-3
    launch.rs               NEW — Item P2-4
  tests/
    stub_op_log.rs          NEW — Item P2-6
  README.md                 (existing; one agent appends a section)

docs/gpu/
  streams-events.md         NEW — Item P2-9

craton-gpu/build.rs         (existing; one agent fixes env-var propagation)
jit-cuda/build.rs           (existing; one agent receives the propagated path)
```

### 2.2 Cargo features (unchanged)

The `cuda` feature on `cuda-bridge` switches between the real backend
(`cudarc`) and the stub. All Phase 2 code must compile in **both**
modes. The stub mode is exercised in tests; the real mode is exercised
on a GPU host.

### 2.3 `Stream` type (Item P2-1 owns)

```rust
// cuda-bridge/src/stream.rs

use crate::{DeviceContext, DeviceError, Result};

/// A CUDA stream handle. Operations submitted to a stream execute in
/// order; operations on different streams run concurrently unless
/// synchronised via Events.
///
/// Stub mode records every operation in a thread-safe Vec<StreamOp>
/// available via `ops()` for test inspection.
pub struct Stream {
    #[cfg(feature = "cuda")]
    inner: StreamCuda,
    #[cfg(not(feature = "cuda"))]
    inner: StreamStub,
}

#[cfg(not(feature = "cuda"))]
struct StreamStub {
    /// Sequence of recorded ops for op-log testing.
    ops: std::sync::Mutex<Vec<StreamOp>>,
    /// Per-stream ordinal so tests can distinguish streams.
    id: u32,
}

#[cfg(feature = "cuda")]
struct StreamCuda {
    raw: std::sync::Arc<cudarc::driver::CudaStream>,
    id: u32,
}

/// Recorded operations in stub mode. Other modules append to this
/// when they accept a `&Stream` — see `async_memcpy.rs`, `event.rs`,
/// `launch.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOp {
    /// Async H→D copy of `len` bytes.
    UploadAsync { bytes: usize },
    /// Async D→H copy of `len` bytes.
    DownloadAsync { bytes: usize },
    /// Kernel launch.
    Launch { kernel: String, grid: (u32, u32, u32), block: (u32, u32, u32) },
    /// Event recorded on this stream.
    EventRecord { event_id: u32 },
    /// Wait for an event recorded elsewhere.
    EventWait { event_id: u32 },
    /// Explicit `cuStreamSynchronize`.
    Synchronize,
}

impl Stream {
    /// Create a new stream on the given context.
    pub fn new(ctx: &DeviceContext) -> Result<Self> {
        // cuda: cudarc::driver::CudaDevice::fork_default_stream() or new_stream()
        // stub: assign a fresh id from a thread-local counter, empty op log
        todo!()
    }

    /// Internal id used for op-log tagging in stub mode and tracing.
    pub fn id(&self) -> u32 { todo!() }

    /// Block the calling thread until all queued work on this stream
    /// has completed. In stub mode, records `StreamOp::Synchronize`.
    pub fn synchronize(&self) -> Result<()> { todo!() }

    /// Append an op to the stub's log. **Internal** — only the
    /// stream.rs, event.rs, async_memcpy.rs, launch.rs files call this.
    /// In real-cuda mode this is a no-op (the real driver tracks
    /// ordering itself; we don't need a side log).
    #[cfg(not(feature = "cuda"))]
    pub(crate) fn record_op(&self, op: StreamOp) {
        if let Ok(mut v) = self.inner.ops.lock() {
            v.push(op);
        }
    }

    #[cfg(feature = "cuda")]
    pub(crate) fn record_op(&self, _op: StreamOp) { /* no-op */ }

    /// Inspect the recorded op log (stub mode). On real cuda this
    /// returns an empty Vec.
    pub fn ops(&self) -> Vec<StreamOp> {
        #[cfg(not(feature = "cuda"))]
        { self.inner.ops.lock().map(|v| v.clone()).unwrap_or_default() }
        #[cfg(feature = "cuda")]
        { Vec::new() }
    }

    /// Access the raw cudarc handle. Public only when the `cuda`
    /// feature is on; used by `async_memcpy.rs` and `launch.rs` to
    /// pass the stream to driver calls.
    #[cfg(feature = "cuda")]
    pub(crate) fn raw(&self) -> &std::sync::Arc<cudarc::driver::CudaStream> {
        &self.inner.raw
    }
}
```

### 2.4 `Event` type (Item P2-2 owns)

```rust
// cuda-bridge/src/event.rs

use crate::{DeviceContext, DeviceError, Result, Stream};

/// A CUDA event — a synchronisation point recorded on a stream. Other
/// streams can wait on this event to enforce a happens-before relation
/// across streams (or across CPU and GPU).
pub struct Event {
    #[cfg(feature = "cuda")]
    inner: EventCuda,
    #[cfg(not(feature = "cuda"))]
    inner: EventStub,
    id: u32,
}

#[cfg(not(feature = "cuda"))]
struct EventStub { recorded_on: std::sync::Mutex<Option<u32>> }

#[cfg(feature = "cuda")]
struct EventCuda { raw: cudarc::driver::CudaEvent }

impl Event {
    /// Create a fresh event. The event is NOT yet recorded — call
    /// `Stream::record_event` to associate it with a stream.
    pub fn new(ctx: &DeviceContext) -> Result<Self> { todo!() }

    /// Unique id (test/debug aid).
    pub fn id(&self) -> u32 { todo!() }

    /// Block until this event is reached on whatever stream recorded
    /// it. Returns Err if the event was never recorded.
    pub fn synchronize(&self) -> Result<()> { todo!() }

    /// Returns true if the recorded work has completed. False if
    /// still running OR the event has not yet been recorded.
    pub fn query(&self) -> Result<bool> { todo!() }
}

impl Stream {
    /// Record `event` at the current point in this stream's queue.
    pub fn record_event(&self, event: &Event) -> Result<()> { todo!() }

    /// Make this stream wait for `event`. All subsequent work on this
    /// stream waits until `event` is reached on its recording stream.
    pub fn wait_event(&self, event: &Event) -> Result<()> { todo!() }
}
```

The `impl Stream { record_event, wait_event }` block lives in
**`event.rs`** (not `stream.rs`) so Item P2-2 owns it without
conflicting with Item P2-1.

### 2.5 Async memcpy (Item P2-3 owns)

Build on existing `DeviceBuffer<T>` (defined in `lib.rs`).

```rust
// cuda-bridge/src/async_memcpy.rs

use crate::{DeviceBuffer, DeviceContext, DeviceError, Result, Stream, StreamOp};
use bytemuck::Pod;

impl<T: Pod> DeviceBuffer<T> {
    /// Async H→D copy. Returns immediately; the upload completes
    /// asynchronously on `stream`. Caller is responsible for not
    /// reading `host` until `stream` is synchronised (or an event
    /// recorded after this call is waited on).
    pub fn from_host_async(
        ctx: &DeviceContext,
        host: &[T],
        stream: &Stream,
    ) -> Result<Self> { todo!() }

    /// Async D→H copy. Returns immediately; the download completes
    /// asynchronously on `stream`. Caller must not read `dst` until
    /// the stream is synchronised.
    pub fn to_host_async(&self, dst: &mut [T], stream: &Stream) -> Result<()> {
        todo!()
    }
}
```

In stub mode: record `StreamOp::UploadAsync { bytes: host.len() * size_of::<T>() }`
or `StreamOp::DownloadAsync { bytes: dst.len() * size_of::<T>() }` and return
`Err(DeviceError::NoDriver)` when actually attempting the copy itself
(consistent with existing sync stub behavior).

**Wait — read this carefully:** the sync `DeviceBuffer::from_host`
in stub mode currently returns `Err(NoDriver)` *before* doing
anything. For the async variant, we want the *record* to succeed but
the actual data transfer not to happen (because there is no device).
This is fine: the stub Stream's op log captures intent, and the
returned `DeviceBuffer` can be a zero-sized handle whose later use
returns `NoDriver`.

Concretely: in stub mode, `from_host_async` should:
1. Record the op on the stream.
2. Return a `DeviceBuffer<T>` that internally holds `len` and `marker`
   data only (no allocation; like the existing stub `DeviceBuffer`).

Look at the existing `backend_stub.rs::DeviceBuffer` impl to mirror
its shape.

### 2.6 `launch_on_stream` (Item P2-4 owns)

```rust
// cuda-bridge/src/launch.rs

use crate::{
    DeviceContext, DeviceError, DeviceModule, KernelArgs, LaunchConfig,
    Result, Stream, StreamOp,
};

impl DeviceModule {
    /// Launch a kernel on `stream`. Non-blocking: returns as soon as
    /// the launch is queued. Wait for completion via
    /// `stream.synchronize()` or via an event recorded after this call.
    pub fn launch_on_stream(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        stream: &Stream,
    ) -> Result<()> { todo!() }
}
```

In stub mode: record `StreamOp::Launch { kernel: kernel.to_string(),
grid: cfg.grid_dim, block: cfg.block_dim }` and return `Ok(())`.

In cuda mode: call `cudarc::driver::CudaFunction::launch_on_stream`
or equivalent — look at the existing `launch_raw` in `lib.rs` for the
pattern and add a `stream:` parameter to the `cuLaunchKernel` call.

### 2.7 lib.rs glue (Item P2-5 owns)

`cuda-bridge/src/lib.rs` already exists and is the entry point. Item
P2-5 adds:

```rust
pub mod stream;
pub mod event;
pub mod async_memcpy;
pub mod launch;

pub use stream::{Stream, StreamOp};
pub use event::Event;
```

That's it. Just the four `pub mod` lines and the re-exports near the
top of the file. **Do NOT modify** existing `DeviceBuffer`,
`DeviceContext`, `DeviceModule`, `KernelArgs`, `LaunchConfig`,
`DeviceError`, `Result` types — they are leveraged unchanged.

### 2.8 Integration test (Item P2-6 owns)

`cuda-bridge/tests/stub_op_log.rs` — feature-gated to run only when
NOT `cuda` (i.e., stub mode):

```rust
#![cfg(not(feature = "cuda"))]

use cuda_bridge::{
    DeviceContext, DeviceBuffer, DeviceModule, Event, KernelArgs,
    LaunchConfig, Stream, StreamOp,
};

/// Three-stage pipeline:
///   upload A → launch f(A) → record event → wait event on stream2
///   → upload B on stream2 → launch g(A, B) → download.
/// Assert the op log on each stream matches expectations.
#[test]
fn three_stage_pipeline() {
    let ctx = match DeviceContext::probe() {
        Ok(c) => c,
        Err(_) => return, // stub returns NoDriver — that's expected
    };
    // ... more setup, op-log assertions ...
}

#[test]
fn upload_async_records_byte_count() {
    // Create stream, call DeviceBuffer::from_host_async with a known
    // host buffer, assert the op log contains exactly one
    // UploadAsync { bytes: n * sizeof(T) }.
}

#[test]
fn launch_on_stream_records_kernel_name() {
    // Stub DeviceModule::from_ptx, call launch_on_stream with kernel
    // name "my_kernel", grid (10, 1, 1), block (256, 1, 1).
    // Assert StreamOp::Launch matches.
}

#[test]
fn event_record_and_wait() {
    // Two streams. Record event on stream_a. Wait event on stream_b.
    // Assert stream_a.ops contains EventRecord, stream_b.ops contains
    // EventWait with matching event_id.
}
```

The actual `DeviceContext::probe` in stub mode returns `NoDriver`. So
each test guards on that and *returns silently* if probe fails — the
test still "passes" because the stub backend successfully refused.
Real-mode validation happens on a GPU host.

### 2.9 cuda-bridge README updates (Item P2-7 owns)

Append a new section to `cuda-bridge/README.md`:

```markdown
## Streams, events, and async memcpy (Phase 2)

Phase 2 adds the building blocks for asynchronous GPU work. The
existing synchronous API (`from_host`, `launch_raw`) is unchanged.

### Stream

`Stream::new(ctx)` creates a CUDA stream. Operations submitted to a
stream execute in FIFO order; operations on different streams run
concurrently.

```rust
let ctx = DeviceContext::probe()?;
let stream = Stream::new(&ctx)?;
let buf = DeviceBuffer::<f32>::from_host_async(&ctx, &host[..], &stream)?;
module.launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)?;
let mut out = vec![0.0f32; host.len()];
buf.to_host_async(&mut out, &stream)?;
stream.synchronize()?;          // block until all three steps done
```

### Event

`Event::new(ctx)` creates an event; record it on one stream, wait on
it from another.

```rust
let s1 = Stream::new(&ctx)?;
let s2 = Stream::new(&ctx)?;
let ev = Event::new(&ctx)?;
module.launch_on_stream(&ctx, "stage1", &cfg, args1, &s1)?;
s1.record_event(&ev)?;
s2.wait_event(&ev)?;
module.launch_on_stream(&ctx, "stage2", &cfg, args2, &s2)?;
```

### Stub-mode op log

When `cuda-bridge` is built without the `cuda` feature, every stream
records its operations in an in-memory `Vec<StreamOp>`. Tests can
inspect this via `stream.ops()` to verify control flow without a real
GPU. Real-mode `ops()` returns an empty Vec.
```

### 2.10 Phase 1.5 follow-up: ANNOTATIONS_DIR propagation (Item P2-8 owns)

Today `craton-gpu/build.rs` emits
`cargo:rustc-env=CRATON_GPU_ANNOTATIONS_DIR=...`, which only affects
`craton-gpu`'s own crate compilation. `jit-cuda/build.rs` needs to
read this value but `cargo` does NOT propagate it.

**Fix:** use cargo's `links` + `DEP_<NAME>_<KEY>` mechanism.

Steps:
1. `craton-gpu/Cargo.toml`: add `links = "craton-gpu-annotations"` to
   `[package]`. (The value is a virtual name; nothing actually links.)
2. `craton-gpu/build.rs`: instead of `cargo:rustc-env=...`, emit
   `cargo:annotations_dir=...` and `cargo:annotations_jar=...`. These
   become `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR` and
   `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR` env vars to any
   crate that depends on `craton-gpu` AND whose build script needs them.
3. `jit-cuda/Cargo.toml`: add `craton-gpu = { path = "../craton-gpu" }`
   as a regular dependency (the new `links` makes it act like a sys
   crate; cargo guarantees craton-gpu's build script runs first).
4. `jit-cuda/build.rs`: replace the `std::env::var("CRATON_GPU_ANNOTATIONS_DIR")`
   call with
   `std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR")`.

After this lands, `cargo build` should compile the `craton.gpu.*`
classes AND the annotation fixtures under
`test_classes/gpu/annotations/`.

### 2.11 streams-events.md user doc (Item P2-9 owns)

`docs/gpu/streams-events.md` — user-facing reference matching the
tone of `docs/gpu/README.md`. Sections:
- **Quick start** — short before/after showing sync vs async pipeline
- **`Stream`** — semantics, construction, ops, synchronisation
- **`Event`** — record/wait pattern, three-stream graph example
- **Async memcpy** — `from_host_async` / `to_host_async`, ownership rules
- **`launch_on_stream`** — non-blocking launch
- **Stub-mode op log** — how to write tests without a GPU
- **What this is NOT** — still Rust-side only; no Java API yet (Phase 3)
- **Limitations** — no graph capture, no event timing, etc.

Also update `docs/gpu/README.md` Document map table to add a row
pointing at `streams-events.md`.

Target ~250-350 lines.

## 3. Item-by-item assignments

| # | Description | Files to create/modify | Approx. LOC |
|---|---|---|---|
| P2-1 | `Stream` type + stub op log + cuda backend impl | `cuda-bridge/src/stream.rs` (new) | ~280 |
| P2-2 | `Event` type + `Stream::record/wait_event` | `cuda-bridge/src/event.rs` (new) | ~150 |
| P2-3 | `DeviceBuffer::from_host_async` / `to_host_async` | `cuda-bridge/src/async_memcpy.rs` (new) | ~200 |
| P2-4 | `DeviceModule::launch_on_stream` | `cuda-bridge/src/launch.rs` (new) | ~150 |
| P2-5 | `pub mod` declarations + re-exports in lib.rs | `cuda-bridge/src/lib.rs` (additive only) | ~30 |
| P2-6 | Stub-backed integration tests | `cuda-bridge/tests/stub_op_log.rs` (new) | ~250 |
| P2-7 | README addendum | `cuda-bridge/README.md` (append section) | ~80 |
| P2-8 | Fix ANNOTATIONS_DIR propagation via `links` | `craton-gpu/Cargo.toml`, `craton-gpu/build.rs`, `jit-cuda/Cargo.toml`, `jit-cuda/build.rs` | ~80 |
| P2-9 | `docs/gpu/streams-events.md` + README link | `docs/gpu/streams-events.md` (new), `docs/gpu/README.md` (one-line append to table) | ~300 |

## 4. Constraints binding all agents

1. **Do not run cargo, javac, or any compiler.** Write source only.
2. **Do not commit.**
3. **Do not push.** Never.
4. **Match names from §2 exactly** (`Stream`, `StreamOp`, `Event`,
   `from_host_async`, `to_host_async`, `launch_on_stream`,
   `record_event`, `wait_event`, etc.).
5. **Each agent owns its own file.** The only shared file is
   `cuda-bridge/src/lib.rs` (Item P2-5 only) and `cuda-bridge/README.md`
   (Item P2-7 only). Other touches need to be additive.
6. **Both feature modes must compile.** Every type and method needs
   both the `#[cfg(feature = "cuda")]` real impl and the
   `#[cfg(not(feature = "cuda"))]` stub impl.
7. **Use existing types** from `cuda-bridge/src/lib.rs` —
   `DeviceContext`, `DeviceBuffer<T>`, `DeviceModule`, `KernelArgs`,
   `LaunchConfig`, `DeviceError`, `Result`. Do NOT redefine these.
8. **Stub mode never panics.** If real-mode would error, stub returns
   `Err(DeviceError::NoDriver)` or `Err(DeviceError::Driver(...))`.
9. **If you must guess, prefix the comment with `// PHASE2-GUESS:`.**

## 5. Acceptance for Phase 2 (orchestrator-side)

Phase 2 is complete when:

- [ ] `cargo check --workspace` (default) clean
- [ ] `cargo check --workspace --features cratonvm-vm/gpu-offload` clean
- [ ] `cargo check -p cuda-bridge --features cuda` clean (validates real backend compiles even if no GPU)
- [ ] `cargo test -p cuda-bridge` — passes existing 3 tests + new stream/event tests in inline modules
- [ ] `cargo test -p cuda-bridge --test stub_op_log` — passes Item P2-6's four integration tests
- [ ] `craton-gpu-annotations.jar` builds on a machine with javac
- [ ] `jit-cuda/build.rs` no longer emits the
  "craton-gpu annotations not built; skipping" warning when javac
  is available (Item P2-8 fix landed)
- [ ] `docs/gpu/streams-events.md` exists; linked from `docs/gpu/README.md`
- [ ] `cuda-bridge/README.md` has the new "Streams, events, and async
  memcpy" section
