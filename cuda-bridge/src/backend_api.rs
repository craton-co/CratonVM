// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The contract a device backend must satisfy.
//!
//! # Why this exists
//!
//! `lib.rs` selects a backend by module alias:
//!
//! ```text
//! #[cfg(feature = "cuda")]        use backend_cuda  as backend;  // cudarc
//! #[cfg(feature = "cuda-oxide")]  use backend_oxide as backend;  // cuda-core
//! #[cfg(not(feature = "gpu-driver"))] use backend_stub as backend;
//! ```
//!
//! That is a real seam, and until 2026-09-04 it was an entirely
//! **unwritten** one. The modules happened to provide
//! `DeviceContextInner`, `DeviceModuleInner`, `DeviceBufferInner<T>`,
//! `PinnedHostInner<T>`, `probe_device` and `driver_cuda_version`, and
//! nothing anywhere required them to agree about it. Only one module
//! compiles per build, so a divergence is invisible until somebody
//! builds the other configuration.
//!
//! They HAD diverged. `DeviceModuleInner::from_ptx` takes
//! `kernel_names: &[&'static str]` in `backend_cuda` (cudarc 0.13 stores
//! the slice for the module's lifetime) and `&[&str]` in
//! `backend_stub`. The stub accepts strictly more than the real backend
//! does, so code written against the stub can fail to compile against
//! CUDA. That is the class of drift these traits turn into a compile
//! error.
//!
//! # What is deliberately NOT here
//!
//! `backend_cuda::DeviceContextInner` also exposes `device()`,
//! `event_pool()`, `alloc_bytes()` and `slice_from_raw()`, which return
//! `Arc<CudaDevice>`, `Arc<EventPool>`, `CUdeviceptr` and
//! `CudaSlice<T>`. Those are vendor types and they stay off the trait:
//! putting them on it would make the "abstraction" a synonym for cudarc
//! and guarantee no second backend could ever satisfy it. They remain
//! inherent methods on the CUDA backend, reachable only from code
//! already inside `#[cfg(feature = "cuda")]`.
//!
//! This is therefore the PORTABLE surface, not the whole surface. The
//! remaining `cudarc` references in `graph.rs` are what is left; see the
//! crate docs for the running count.
//!
//! # The second half, added when the third backend arrived
//!
//! [`DriverApi`] closes the hole this module's own text described. The
//! thin driver operations `stream.rs` and `event.rs` need — twelve of
//! them — lived in a per-backend `pub(crate) mod drv` whose only
//! guarantee was that `backend_cuda.rs` and `backend_oxide.rs` happened
//! to declare the same names with the same signatures. That is exactly
//! the arrangement the paragraphs above call a drift hazard, and it had
//! no compile-time check at all.
//!
//! Two backends could sustain it because they are the same driver:
//! `drv`'s handles were spelled `CUstream` and `CUevent` in both, and
//! `lib.rs` aliased whichever was compiled to a shared `drvsys` with a
//! comment observing that "these are the SAME driver types in both
//! cases". A third backend on a different driver has no `CUstream`, so
//! that alias could not survive and the twelve functions had to acquire
//! a real contract on the way out.
//!
//! The handles are now associated types. `stream.rs`/`event.rs` name
//! them only as `drv::RawStream` / `drv::RawEvent`, and `crate::drv` is
//! one generic forwarding module rather than one copy per backend — so
//! the twelve functions exist once and a backend that gets one wrong
//! fails to compile instead of failing at run time on hardware nobody
//! has.

use crate::{DeviceCaps, KernelArgs, LaunchConfig, Result};

/// Device discovery and driver interrogation: the parts a backend must
/// answer before any context exists.
pub(crate) trait BackendApi {
    /// The context type this backend hands out.
    type Context: DeviceContextApi;

    /// The module type this backend loads PTX into.
    type Module: DeviceModuleApi<Ctx = Self::Context, Stream = Self::Stream>;

    /// The stream type a launch is ordered against. `Arc<CudaStream>` on
    /// the CUDA backend; a unit-like placeholder on a backend with no
    /// device, which is why this is an associated type rather than a
    /// concrete one.
    type Stream;

    /// Capabilities of `device_ordinal`, or an error describing why the
    /// device is unusable. Must not panic on a machine with no driver:
    /// the no-device answer is an `Err`, and every caller treats it as
    /// "run on the CPU instead".
    fn probe_device(device_ordinal: u32) -> Result<DeviceCaps>;

    /// Which backend this is. Reported by `--gpu-info` and used by the
    /// VM to pick a lowering target; never branched on for behaviour
    /// inside this crate, where the compiled-in module already decides.
    fn backend_kind() -> crate::BackendKind;

    /// The installed toolchain's version, in whatever terms this
    /// backend counts.
    ///
    /// On CUDA this is `cuDriverGetVersion`, and it is a ceiling: a
    /// module whose PTX `.version` exceeds what the driver can parse
    /// fails to load, and the VM silently falls back to the CPU. On HIP
    /// it is the runtime and hiprtc versions, which are recorded rather
    /// than clamped against -- see [`crate::ToolchainVersion`] for why
    /// the two families cannot share one `u32`.
    fn toolchain_version() -> Result<crate::ToolchainVersion>;
}

/// A live device context.
pub(crate) trait DeviceContextApi: Sized {
    /// Acquire the context for `device_ordinal`.
    fn new(device_ordinal: u32) -> Result<Self>;

    /// Block until every stream on this context has drained.
    fn synchronize(&self) -> Result<()>;

    /// Make this context current on the calling thread. Required before
    /// any driver call from a thread that has not made one before,
    /// because the driver's current-context is thread-local.
    fn bind_to_thread(&self) -> Result<()>;

    /// Record `event` on whatever stream the backend performs its
    /// allocations on.
    ///
    /// This exists because `DeviceBuffer::zeros` issues an asynchronous
    /// memset: without an event recorded after it, a kernel launched on
    /// another stream can observe the buffer before the zeroing lands.
    /// That was a measured corruption, not a theoretical one.
    fn record_alloc_event(&self, event: &crate::Event) -> Result<()>;
}

/// A loaded module and the kernels in it.
pub(crate) trait DeviceModuleApi: Sized {
    /// The context type this module was loaded into.
    type Ctx;

    /// The stream type `launch_raw_on_stream` orders against.
    type Stream;

    /// Load `image` and resolve `kernel_names`.
    ///
    /// The parameter used to be `ptx: &str`, which named one vendor's
    /// kernel format in the contract every backend has to satisfy. A
    /// backend that cannot consume the variant it is handed must return
    /// [`crate::DeviceError::UnsupportedImage`] rather than `NoDriver`:
    /// the first is a target-selection bug, the second is a machine
    /// without a GPU, and they call for opposite responses.
    ///
    /// `&'static str` on the names is not gratuitous: cudarc 0.13 keeps
    /// the name slice for the lifetime of the loaded module. A backend
    /// that does not need that is free to ignore it, but the trait takes
    /// the STRICTER lifetime so a backend that does need it can be
    /// written -- the reverse would make cudarc unimplementable.
    fn from_kernel_image(
        ctx: &Self::Ctx,
        image: &crate::KernelImage<'_>,
        module_name: &str,
        kernel_names: &[&'static str],
    ) -> Result<Self>;

    /// The block size the driver's occupancy calculator prefers for
    /// `kernel`, or `None` when the backend cannot answer. `None` is a
    /// legitimate answer, not a failure: the caller falls back to a
    /// fixed block size.
    fn optimal_block_size(&self, ctx: &Self::Ctx, kernel: &str) -> Option<u32>;

    /// Launch `kernel` on `stream` with `cfg` and `args`.
    ///
    /// Returns once the launch is ENQUEUED, not once it completes; the
    /// caller synchronises through an event or the stream.
    fn launch_raw_on_stream(
        &self,
        ctx: &Self::Ctx,
        stream: &Self::Stream,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()>;
}

/// The twelve thin driver operations `stream.rs` and `event.rs` need,
/// and the two handle types they are expressed over.
///
/// # Why the handles are associated types
///
/// A stream handle is `CUstream` on both CUDA backends and
/// `hipStream_t` on HIP. Shared code never needs to know which — it
/// only moves the value from `stream_raw` to `event_record` and back —
/// so the concrete type is the backend's business and
/// `stream.rs`/`event.rs` name it as [`DriverApi::RawStream`].
///
/// `Copy` is required because these are passed by value into `unsafe`
/// FFI calls; nothing stronger is, and deliberately so. Both drivers
/// spell these handles as raw pointers, which are neither `Send` nor
/// `Sync`, so demanding those bounds here would be unimplementable.
/// Thread-safety is asserted one level up, on the `Stream` and `Event`
/// wrappers, where the justification actually lives: the driver permits
/// handle use from any thread once the context is bound, which is a
/// statement about the driver and not about the pointer.
///
/// # Why `launch_host_func` takes the closure
///
/// It used to take a vendor callback-ABI type (`CUhostFn`) and a raw
/// `*mut c_void`, which meant the trampoline that unpacks the closure —
/// including its `catch_unwind` guard against unwinding into
/// driver-owned code — lived in shared code and named a vendor type to
/// do it. Taking the `Box` instead moves the trampoline into each
/// backend, where the ABI it must match is known, and leaves no vendor
/// type on the contract. The double-box, the exactly-once ownership
/// transfer and the reclaim-on-synchronous-failure rule are unchanged;
/// they are just stated per backend now.
#[cfg(feature = "gpu-driver")]
pub(crate) trait DriverApi {
    /// Keeps the owning primary context alive for a handle's lifetime.
    type DeviceHandle: Clone;
    /// An owned stream. Dropping it is the backend's business.
    type StreamHandle;
    /// The raw stream handle an FFI call takes.
    type RawStream: Copy;
    /// The raw event handle an FFI call takes.
    type RawEvent: Copy;

    /// Bind `device`'s context to the calling thread.
    ///
    /// Required before any driver call from a thread that has not made
    /// one, because the driver's current context is thread-local. Every
    /// stream- and event-driving method in `stream.rs`/`event.rs` opens
    /// with this.
    fn device_bind(device: &Self::DeviceHandle) -> Result<()>;

    /// A new non-blocking stream ordered after all work already
    /// submitted to the device's default stream.
    ///
    /// This is a fork point in the stream DAG: the new stream observes
    /// everything queued on the default stream at construction time,
    /// and nothing queued after it. A backend whose stream-creation
    /// call does not imply that ordering has to establish it here.
    fn fork_default_stream(device: &Self::DeviceHandle) -> Result<Self::StreamHandle>;

    /// The raw handle behind an owned stream.
    fn stream_raw(stream: &Self::StreamHandle) -> Self::RawStream;

    /// A value equal for two handles exactly when they name the same
    /// stream.
    ///
    /// Used only to elide a wait a stream would place on itself. It is
    /// an identity token and nothing else — never dereferenced, never
    /// compared for ordering, and no backend should read meaning into
    /// the number.
    fn stream_token(stream: Self::RawStream) -> usize;

    /// Create a timing-disabled event. The bridge never measures
    /// elapsed GPU time, so it does not pay for the timing variant.
    fn event_create() -> Result<Self::RawEvent>;

    /// # Safety
    /// `event` and `stream` must be live and share the bound context.
    unsafe fn event_record(event: Self::RawEvent, stream: Self::RawStream) -> Result<()>;

    /// `Ok(true)` when the event has fired, `Ok(false)` while it is
    /// still in flight.
    ///
    /// Both drivers report "still in flight" as an error code
    /// (`CUDA_ERROR_NOT_READY` / `hipErrorNotReady`). Folding it into
    /// `Ok(false)` is the backend's job precisely so no caller has to
    /// grep for a vendor-specific variant.
    ///
    /// # Safety
    /// `event` must be live and its context bound.
    unsafe fn event_query(event: Self::RawEvent) -> Result<bool>;

    /// # Safety
    /// `event` must be live and its context bound.
    unsafe fn event_synchronize(event: Self::RawEvent) -> Result<()>;

    /// # Safety
    /// `event` must be uniquely owned here and its context bound.
    unsafe fn event_destroy(event: Self::RawEvent);

    /// Make `stream` wait for `event` without blocking the host.
    ///
    /// # Safety
    /// Both handles must be live and share the bound context.
    unsafe fn stream_wait_event(stream: Self::RawStream, event: Self::RawEvent) -> Result<()>;

    /// # Safety
    /// `stream` must be live and its context bound.
    unsafe fn stream_synchronize(stream: Self::RawStream) -> Result<()>;

    /// Enqueue `f` to run on the host once everything already submitted
    /// to `stream` has completed.
    ///
    /// The implementation owns `f` from here: on success the driver's
    /// exactly-once callback must consume it, and on a synchronous
    /// submission failure the implementation must drop it rather than
    /// leak it. `f` must not re-enter the driver — see
    /// `Stream::add_host_callback`, which documents the rule for
    /// callers — and a panic inside it must not unwind across the
    /// callback ABI.
    ///
    /// # Safety
    /// `stream` must be live in the bound context.
    unsafe fn launch_host_func(stream: Self::RawStream, f: Box<dyn FnOnce() + Send>) -> Result<()>;
}
