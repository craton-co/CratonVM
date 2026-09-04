// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The contract a device backend must satisfy.
//!
//! # Why this exists
//!
//! `lib.rs` selects a backend by module alias:
//!
//! ```ignore
//! #[cfg(feature = "cuda")]     use backend_cuda as backend;
//! #[cfg(not(feature = "cuda"))] use backend_stub as backend;
//! ```
//!
//! That is a real seam, and until 2026-09-04 it was an entirely
//! **unwritten** one. Both modules happened to provide
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
//! remaining `cudarc` references in `stream.rs`, `event.rs`, `graph.rs`
//! and `lib.rs` are the next thing to move behind it; see the crate
//! docs for the running count.

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

    /// The driver's CUDA version as `major * 1000 + minor * 10`, which
    /// is what `cuDriverGetVersion` reports. Used to clamp the PTX ISA
    /// version the emitter targets -- a module whose `.version` exceeds
    /// what the driver accepts fails to load, and the VM silently falls
    /// back to the CPU.
    fn driver_cuda_version() -> Result<u32>;
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

    /// Load `ptx` and resolve `kernel_names`.
    ///
    /// `&'static str` is not gratuitous: cudarc 0.13 keeps the name
    /// slice for the lifetime of the loaded module. A backend that does
    /// not need that is free to ignore it, but the trait takes the
    /// STRICTER lifetime so a backend that does need it can be written
    /// -- the reverse would make cudarc unimplementable.
    fn from_ptx(
        ctx: &Self::Ctx,
        ptx: &str,
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
