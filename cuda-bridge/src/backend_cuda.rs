// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! cudarc-backed real CUDA backend.
//!
//! Only compiled when the `cuda` Cargo feature is enabled.
//!
//! This file intentionally keeps the surface narrow: device discovery,
//! PTX module loading, allocation, memcpy, and `cuLaunchKernel`. The
//! moment cudarc's API moves under us, all the fan-out stays in this
//! file — the public crate API in `lib.rs` is unchanged.
//!
//! AUDIT 2026-05-17 (PERF Fix 1): the context now owns THREE streams —
//! `copy_h2d`, `compute`, `copy_d2h` — so H→D transfer, kernel launch,
//! and D→H transfer can overlap pairwise (kernel runs while the next
//! launch's H→D upload is in flight, etc.). Inter-stream ordering is
//! enforced with cudarc events:
//!
//!   H→D  ── record(e_h2d) ──>  compute waits on e_h2d
//!   compute ── record(e_k) ──>  copy_d2h waits on e_k
//!
//! See `DeviceContextInner::new` for stream construction and the
//! `from_host` / `launch_raw` / `to_host` methods for the record/wait
//! choreography.
//!
//! AUDIT 2026-05-24 (C32 stream-port fix): completed the cudarc 0.13
//! stream port. Three half-finished bugs are addressed here:
//!   * `from_host` now submits the H→D copy via
//!     `result::memcpy_htod_async` on `copy_h2d` and records `e_h2d`
//!     so subsequent compute-stream launches order behind it without
//!     blocking the host.
//!   * `launch_raw_on_stream` (new) takes an explicit raw `CUstream`
//!     and uses `LaunchAsync::launch_on_stream` against the cudarc-
//!     wrapped user stream, so `DeviceModule::launch_on_stream`
//!     actually launches on the requested stream.
//!   * `to_host_async_raw` (new) issues `result::memcpy_dtoh_async`
//!     on the supplied stream after a `cuStreamWaitEvent` on `e_k`,
//!     letting the caller `await` via their stream's `synchronize`
//!     instead of `cuCtxSynchronize`-blocking the world here.

use crate::{DeviceCaps, DeviceError, KernelArg, KernelArgs, LaunchConfig, Result};
use cudarc::driver::{
    CudaDevice, CudaFunction, CudaSlice, CudaStream, DevicePtr, DeviceRepr, DeviceSlice,
    LaunchAsync, LaunchConfig as CudarcLaunchConfig,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

fn map_err<E: std::fmt::Debug>(stage: &str) -> impl FnOnce(E) -> DeviceError + '_ {
    move |e| DeviceError::Driver(format!("{stage}: {e:?}"))
}

pub(crate) fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
    let dev = CudaDevice::new(device_ordinal as usize)
        .map_err(map_err("CudaDevice::new(device_ordinal)"))?;
    let name = dev.name().map_err(map_err("device name"))?;
    let attr = |a| dev.attribute(a).map_err(map_err("device attribute"));
    let major = attr(
        cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR,
    )?;
    let minor = attr(
        cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR,
    )?;
    // cudarc 0.13 / the CUDA driver API has no
    // `CU_DEVICE_ATTRIBUTE_TOTAL_MEMORY` device attribute — total
    // global memory is queried with `cuDeviceTotalMem` instead. The
    // safe wrapper is `result::device::total_mem`, which takes the raw
    // `CUdevice` handle exposed by `CudaDevice::cu_device()`.
    // SAFETY: `dev` owns a live device handle for the duration of this
    // synchronous query.
    let total_mem = unsafe {
        cudarc::driver::result::device::total_mem(*dev.cu_device())
            .map_err(map_err("total memory"))?
    };
    Ok(DeviceCaps {
        ordinal: device_ordinal,
        name,
        compute_major: major as u32,
        compute_minor: minor as u32,
        total_global_mem: total_mem as u64,
    })
}

/// `cuDriverGetVersion`, as `1000 * major + 10 * minor` (e.g. `12080`
/// for a CUDA 12.8 driver).
///
/// The installed driver's version is the ceiling on the PTX ISA version
/// it can parse, and a module declaring a newer `.version` is rejected
/// exactly as hard as one naming an unknown `.target`. The VM reads this
/// once at `OffloadCache` construction and hands it to
/// `jit_cuda::target::clamp_target_to_isa`; see that module for why both
/// directions need handling.
pub(crate) fn driver_cuda_version() -> Result<u32> {
    // `cuDriverGetVersion` is one of the few driver entry points that is
    // legal before `cuInit`, but cudarc loads the library lazily, so go
    // through `lib()` to make sure the symbol table exists. Any failure
    // is reported rather than papered over: the caller treats an unknown
    // driver version as "do not clamp", which is the pre-existing
    // behaviour.
    let mut version: core::ffi::c_int = 0;
    // SAFETY: `lib()` returns the loaded driver library; `version` is a
    // live, aligned `c_int` the call writes exactly once.
    let res = unsafe { cudarc::driver::sys::lib().cuDriverGetVersion(&mut version) };
    if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
        return Err(DeviceError::NoDriver);
    }
    if version <= 0 {
        return Err(DeviceError::NoDriver);
    }
    Ok(version as u32)
}

/// AUDIT 2026-05-24 (C32 stream-port fix): inter-stream barrier events
/// owned by the context. Each is a `CU_EVENT_DISABLE_TIMING` event
/// created once at context construction and reused for every transfer:
///   * `e_h2d` — recorded on `copy_h2d` after every async upload;
///     `compute` waits on it before launching a kernel that depends on
///     freshly-uploaded inputs.
///   * `e_k` — recorded on `compute` after every kernel launch;
///     `copy_d2h` waits on it before starting an async D→H download.
///
/// Re-recording a CUevent overwrites the prior marker, which matches
/// what we want: a wait_event observes the *most recent* recording.
/// The events live as long as the device they were created on; they
/// are destroyed in `Drop` with the device bound to the current
/// thread (the same teardown pattern `EventCuda::drop` uses).
struct StreamBarriers {
    e_h2d: cudarc::driver::sys::CUevent,
    e_k: cudarc::driver::sys::CUevent,
    dev: Arc<CudaDevice>,
}

// SAFETY: `CUevent` is a raw `*mut CUevent_st` handle. The CUDA driver
// docs permit using an event from any thread that has the owning
// primary context bound; the `dev` field's `Arc<CudaDevice>` keeps the
// primary context alive and `bind_to_thread` (called at every entry on
// the cross-thread-callable methods of `DeviceContextInner`) restores
// the binding on the current thread. The barriers therefore satisfy
// the same conditional `Send`/`Sync` invariant as the rest of the
// bridge — sound only when callers honour the `bind_to_thread`
// contract documented on `DeviceContext` (see SOUND-1 in the C32
// review).
unsafe impl Send for StreamBarriers {}
// SAFETY: the same context-binding and Arc lifetime invariant above permits
// concurrent references; CUDA serializes event operations.
unsafe impl Sync for StreamBarriers {}

impl Drop for StreamBarriers {
    fn drop(&mut self) {
        let _ = self.dev.bind_to_thread();
        // SAFETY: both events were created by this context, remain live and
        // uniquely owned by the dropping barrier after binding the context.
        unsafe {
            let _ = cudarc::driver::result::event::destroy(self.e_h2d);
            let _ = cudarc::driver::result::event::destroy(self.e_k);
        }
    }
}

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    dev: Arc<CudaDevice>,
    /// Default cudarc stream — the original allocator/launch root. Kept
    /// so `synchronize()` can drain it for callers that still hold
    /// buffers created before the three-stream restructure.
    #[allow(dead_code)]
    default: Arc<CudaStream>,
    /// Stream used for host→device memcpys (uploads).
    copy_h2d: Arc<CudaStream>,
    /// Stream used for kernel launches. Waits on `copy_h2d` events
    /// before launching, then records its own event for `copy_d2h`.
    compute: Arc<CudaStream>,
    /// Stream used for device→host memcpys (downloads). Waits on the
    /// compute stream's event before starting any read.
    copy_d2h: Arc<CudaStream>,
    /// AUDIT 2026-05-24 (C32 stream-port fix): inter-stream barrier
    /// events shared between the three copy/compute streams. See
    /// `StreamBarriers` doc for the choreography.
    barriers: Arc<StreamBarriers>,
    /// Free list of `CUevent` handles, recycled instead of destroyed.
    ///
    /// Every kernel submission mints TWO events on this path -- one
    /// inside `DeviceModule::launch_on_stream` (the per-buffer
    /// `last_write` marker) and one in `vm::runtime::offload` (the
    /// submission-completion marker) -- and destroys both when the
    /// submission is finalized. GPULlama3's forward pass makes 453
    /// submissions per token, so that is ~900 `cuEventCreate` /
    /// `cuEventDestroy` pairs per token on a path whose whole per-token
    /// host budget is 14 ms. A `CUevent` carries no per-recording state
    /// worth resetting -- `cuEventRecord` overwrites it, and a
    /// `cuStreamWaitEvent` already issued against it captured its
    /// contents AT THE TIME OF THE CALL (CUDA driver semantics), so a
    /// later re-record cannot retroactively satisfy or break an
    /// outstanding wait. Recycling is therefore sound, and the handle
    /// only ever returns here once the last `Arc<Event>` holding it is
    /// gone.
    event_pool: Arc<EventPool>,
}

/// See [`DeviceContextInner::event_pool`].
///
/// Capped so a pathological burst of concurrent submissions cannot leave
/// an unbounded number of live `CUevent`s parked here for the process's
/// lifetime; past the cap a returning handle is destroyed as before.
pub(crate) struct EventPool {
    free: std::sync::Mutex<Vec<cudarc::driver::sys::CUevent>>,
    dev: Arc<CudaDevice>,
}

/// How many idle events one context parks. Two per in-flight submission,
/// and the offload path warns at 1024 live submissions, so this covers
/// the deepest pipeline the VM tolerates with room to spare.
const EVENT_POOL_CAP: usize = 4096;

// SAFETY: mirrors `EventCuda`'s impls. The pool holds raw `CUevent`
// handles and the `Arc<CudaDevice>` whose primary context owns them;
// every method binds that context before touching a handle.
unsafe impl Send for EventPool {}
// SAFETY: the free list is behind a `Mutex`, and CUDA serializes event
// creation and destruction on the bound context.
unsafe impl Sync for EventPool {}

impl EventPool {
    /// Take a recycled handle, or `None` when the free list is empty.
    ///
    /// Deliberately does NOT create on a miss: the caller is
    /// `Event::new`, which already has the create path and its own
    /// error mapping, and a pool hit must not pay for `bind_to_thread`.
    pub(crate) fn take(&self) -> Option<cudarc::driver::sys::CUevent> {
        self.free
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop()
    }

    /// Return a handle, or destroy it if the pool is full.
    pub(crate) fn put(&self, ev: cudarc::driver::sys::CUevent) {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if free.len() < EVENT_POOL_CAP {
            free.push(ev);
            return;
        }
        drop(free);
        let _ = self.dev.bind_to_thread();
        // SAFETY: the handle is uniquely owned here (its last `Event` has
        // just dropped) and its owning context was bound immediately
        // above.
        unsafe {
            let _ = cudarc::driver::result::event::destroy(ev);
        }
    }
}

impl Drop for EventPool {
    fn drop(&mut self) {
        let _ = self.dev.bind_to_thread();
        for ev in self
            .free
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            // SAFETY: the pool is being dropped, so no `Event` holds any
            // of these handles; the owning context was bound above.
            unsafe {
                let _ = cudarc::driver::result::event::destroy(ev);
            }
        }
    }
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        let dev = CudaDevice::new(device_ordinal as usize).map_err(map_err("CudaDevice::new"))?;
        let default = dev
            .fork_default_stream()
            .map_err(map_err("fork_default_stream"))?;
        // AUDIT 2026-05-17 (PERF Fix 1): create three auxiliary streams
        // for the H2D → compute → D2H pipeline. `fork_default_stream` produces
        // an independent cudarc stream (the cudarc equivalent of
        // `cudaStreamCreate(&s, cudaStreamNonBlocking)`).
        let copy_h2d = dev
            .fork_default_stream()
            .map_err(map_err("fork_default_stream copy_h2d"))?;
        let compute = dev
            .fork_default_stream()
            .map_err(map_err("fork_default_stream compute"))?;
        let copy_d2h = dev
            .fork_default_stream()
            .map_err(map_err("fork_default_stream copy_d2h"))?;
        // AUDIT 2026-05-24 (C32 stream-port fix): create the two barrier
        // events with `CU_EVENT_DISABLE_TIMING` since we never measure
        // elapsed GPU time on them — only use them for `cuEventRecord`
        // / `cuStreamWaitEvent` ordering between the three streams.
        // Must bind the primary context before creating; cudarc's
        // event-create wrapper does not bind for us.
        dev.bind_to_thread().map_err(map_err("bind_to_thread"))?;
        let e_h2d = cudarc::driver::result::event::create(
            cudarc::driver::sys::CUevent_flags::CU_EVENT_DISABLE_TIMING,
        )
        .map_err(map_err("cuEventCreate e_h2d"))?;
        let e_k = cudarc::driver::result::event::create(
            cudarc::driver::sys::CUevent_flags::CU_EVENT_DISABLE_TIMING,
        )
        .map_err(map_err("cuEventCreate e_k"))?;
        Ok(Self {
            dev: dev.clone(),
            default: default.into(),
            copy_h2d: copy_h2d.into(),
            compute: compute.into(),
            copy_d2h: copy_d2h.into(),
            event_pool: Arc::new(EventPool {
                free: std::sync::Mutex::new(Vec::new()),
                dev: dev.clone(),
            }),
            barriers: Arc::new(StreamBarriers { e_h2d, e_k, dev }),
        })
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the primary context to
        // this thread before `cuCtxSynchronize`. `DeviceContext` is
        // `Send + Sync` and may be driven from a worker thread other
        // than the one that constructed it; the CUDA driver model
        // requires the context bound on the calling thread first.
        self.bind_to_thread()?;
        // Drain every stream we own. Any in-flight pipeline stage —
        // upload, kernel, download — must complete before we return.
        //
        // `CudaDevice::synchronize` (`cuCtxSynchronize`) host-blocks the
        // calling thread until ALL streams on the context — `copy_h2d`,
        // `compute`, `copy_d2h`, and the default stream — have drained.
        // (`CudaDevice::wait_for` only makes the default stream wait on
        // another stream and does NOT block the host, so it cannot be
        // used here.)
        self.dev
            .synchronize()
            .map_err(map_err("synchronize device"))?;
        Ok(())
    }

    /// Crate-internal accessor used by `stream.rs` / `event.rs` /
    /// `async_memcpy.rs` to reach the underlying `CudaDevice` without
    /// re-creating it. The returned `Arc` is cheap to clone.
    pub(crate) fn device(&self) -> &Arc<CudaDevice> {
        &self.dev
    }

    /// The context's recycled-`CUevent` free list. See
    /// [`DeviceContextInner::event_pool`].
    pub(crate) fn event_pool(&self) -> &Arc<EventPool> {
        &self.event_pool
    }

    /// The raw handle of the context's shared compute stream, for the
    /// same-stream wait elision in `launch_raw_inner`.
    pub(crate) fn compute_stream_raw(&self) -> cudarc::driver::sys::CUstream {
        self.compute.stream
    }

    /// AUDIT 2026-05-29 (SOUND-1 / H10c): bind this context's primary
    /// context to the calling thread.
    ///
    /// The bridge's `unsafe impl Send + Sync` blocks on `DeviceContext`,
    /// `DeviceBuffer`, `Stream`, `EventCuda`, and `StreamBarriers` are
    /// sound only if every thread that drives a handle has first bound
    /// the device's primary context to itself (the CUDA driver model
    /// requires it). cudarc's `bind_to_thread` is a per-thread TLS check
    /// that no-ops after the first call on a given thread, so calling it
    /// as a prelude to every cross-thread-callable public method is
    /// cheap. This is the single helper every such prelude routes
    /// through (mirrors `Event::new` / `DeviceContextInner::new`).
    pub(crate) fn bind_to_thread(&self) -> Result<()> {
        self.dev.bind_to_thread().map_err(map_err("bind_to_thread"))
    }

    /// AUDIT 2026-05-24 (HIGH correctness): expose `copy_h2d`'s raw
    /// stream handle so callers can `cuEventRecord` an event on the same
    /// stream the H→D copy ran on. Currently unused — `from_host_async`
    /// routes uploads onto the *user* stream (H10a) rather than
    /// `copy_h2d` — but retained as the documented accessor for any
    /// future code that needs to order against the context's dedicated
    /// upload stream.
    #[allow(dead_code)]
    pub(crate) fn copy_h2d_raw(&self) -> cudarc::driver::sys::CUstream {
        self.copy_h2d.stream
    }

    /// AUDIT 2026-05-24 (HIGH correctness): expose `compute`'s raw
    /// stream handle.
    ///
    /// AUDIT 2026-05-29 (HIGH-2 fix): no longer used by
    /// `DeviceModule::launch_on_stream` — that path now launches on (and
    /// records `kernel_done` on) the user `Stream` directly, so it no
    /// longer needs the context's `compute` stream handle. Retained as
    /// the documented accessor for the context's compute stream in case
    /// future cross-stream bookkeeping needs it.
    #[allow(dead_code)]
    pub(crate) fn compute_raw(&self) -> cudarc::driver::sys::CUstream {
        self.compute.stream
    }
}

/// A loaded PTX module. cudarc stores the underlying `CudaModule` in a
/// `BTreeMap` on the device keyed by `module_name`; we retain that name
/// plus the resolved `CudaFunction` handles for each requested kernel.
pub(crate) struct DeviceModuleInner {
    dev: Arc<CudaDevice>,
    module_name: String,
    functions: HashMap<String, CudaFunction>,
}

// AUDIT 2026-05-17 (PERF Fix 2): per-launch arg-marshalling allocations
// (the `ptr_h: Vec<u64>` parallel buffer) used to be freshly heap-
// allocated on every `launch_raw`. Hoist into a thread-local growable
// scratch so the steady-state allocation cost is zero.
//
// Why thread-local: cudarc's launch path is host-driven and short-lived
// — we never hand the scratch off to another thread; growth is bounded
// by the largest kernel's pointer-arg count. `RefCell` is enough because
// the borrow is taken and released entirely inside `launch_raw`.
thread_local! {
    static PTR_SCRATCH: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    /// AUDIT 2026-05-20 (PERF Fix #3): pool for the per-launch
    /// `Vec<*mut c_void>` of marshalled kernel arguments handed to
    /// cudarc's `launch_on_stream`. Previously a fresh heap allocation
    /// per `launch_raw`; pooled here the same way as `PTR_SCRATCH` so
    /// steady-state launches are allocation-free.
    ///
    /// The raw pointers stored here are only ever valid for the duration
    /// of one `launch_raw_inner` call (they point into that call's `args`
    /// and into `PTR_SCRATCH`). The scratch is emptied before being
    /// returned to the thread-local, so no dangling pointer is retained
    /// between launches — only the heap allocation is kept. `*mut c_void`
    /// is not `Send`, but the vec never crosses threads.
    static ARG_SCRATCH: RefCell<Vec<*mut std::ffi::c_void>> =
        const { RefCell::new(Vec::new()) };
}

impl DeviceModuleInner {
    pub(crate) fn from_ptx(
        ctx: &DeviceContextInner,
        ptx: &str,
        module_name: &str,
        // cudarc 0.13's `CudaDevice::load_ptx` stores the kernel-name
        // slice for the lifetime of the loaded module, so it requires
        // `&[&'static str]`. The public `DeviceModule::from_ptx` entry
        // point keeps a `&[&str]` surface and bridges via leaking.
        kernel_names: &[&'static str],
    ) -> Result<Self> {
        let ptx_owned = cudarc::nvrtc::Ptx::from_src(ptx);
        ctx.dev
            .load_ptx(ptx_owned, module_name, kernel_names)
            .map_err(map_err("load_ptx"))?;
        let mut functions = HashMap::with_capacity(kernel_names.len());
        for &name in kernel_names {
            let func = ctx
                .dev
                .get_func(module_name, name)
                .ok_or_else(|| DeviceError::KernelNotFound(format!("{module_name}::{name}")))?;
            functions.insert(name.to_string(), func);
        }
        Ok(Self {
            dev: ctx.dev.clone(),
            module_name: module_name.to_string(),
            functions,
        })
    }

    pub(crate) fn launch_raw(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        // Round-7 PERF Fix 3: conservative default — assume a D→H copy
        // follows so callers that do read back results stay correctly
        // ordered, recording the per-buffer kernel-completion event a
        // later `to_host` waits on. (`launch_raw_inner` still takes a
        // `needs_d2h_sync` flag for symmetry / future fire-and-forget
        // variants, but the only public entry today always passes
        // `true`.)
        self.launch_raw_inner(ctx, kernel, cfg, args, /* needs_d2h_sync */ true)
    }

    /// Query the driver for the kernel's occupancy-optimal block size.
    ///
    /// Round-8 fix for the `LaunchConfig::elementwise` hardcoded-256
    /// TODO. Wraps cudarc's
    /// `CudaFunction::occupancy_max_potential_block_size` (which fans
    /// out to `cuOccupancyMaxPotentialBlockSize` in the driver). The
    /// returned size assumes zero dynamic shared memory and no block-
    /// size ceiling, which matches every kernel the bridge currently
    /// launches; the caller (`DeviceModule::elementwise_for_kernel`)
    /// falls back to 256 if this returns `None`.
    ///
    /// `ctx` is accepted but unused today — cudarc 0.13 reads the
    /// context from `CudaFunction` directly. Kept on the signature so
    /// adding context-sensitive autotune later (e.g. binding the
    /// driver query to a non-primary context) does not break callers.
    pub(crate) fn optimal_block_size(
        &self,
        _ctx: &DeviceContextInner,
        kernel: &str,
    ) -> Option<u32> {
        // cudarc's `occupancy_max_potential_block_size` wants an
        // `extern "C" fn(block_size) -> usize` for dynamic-smem sizing.
        // We have no dynamic smem, so the callback always returns 0.
        extern "C" fn zero_smem(_block_size: std::ffi::c_int) -> usize {
            0
        }
        let func = self.functions.get(kernel)?;
        // Pass `0` as block_size_limit to let the driver pick freely;
        // `None` flags use CU_OCCUPANCY_DEFAULT.
        match func.occupancy_max_potential_block_size(zero_smem, 0, 0, None) {
            Ok((_min_grid, block_size)) if block_size > 0 => Some(block_size),
            _ => None,
        }
    }

    /// Launch on the context's `compute` stream. The default-stream
    /// entry point `launch_raw` routes here (with `needs_d2h_sync =
    /// true`). The explicit-stream path (`DeviceModule::launch_on_stream`
    /// → `launch_raw_on_stream`) bypasses this and submits onto the
    /// caller's stream instead.
    fn launch_raw_inner(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        needs_d2h_sync: bool,
    ) -> Result<()> {
        // BUGFIX 2026-06-17 (cuda-backend H10b input-side analogue):
        // snapshot the device-ptr args' `last_write` slots BEFORE the
        // launch consumes `args`. We need these for TWO purposes:
        //   (a) the per-buffer INPUT wait below (gate the compute stream
        //       behind each input's upload/producing-kernel event), and
        //   (b) stamping THIS launch's kernel-completion event back into
        //       each slot afterward (the H10b output-side fix).
        // The snapshot is now UNCONDITIONAL: even a fire-and-forget
        // launch (`needs_d2h_sync == false`) must still order behind its
        // inputs' uploads, so the input wait cannot be gated on the
        // output-sync flag. Only the post-launch write-back (b) remains
        // gated on `needs_d2h_sync`.
        let last_write_slots: Vec<crate::LastWriteSlot> = args
            .raw
            .iter()
            .filter_map(|a| match a {
                KernelArg::DevicePtr { last_write, .. } => Some(last_write.clone()),
                _ => None,
            })
            .collect();

        // BUGFIX 2026-06-17 (cuda-backend): wait on each input buffer's
        // PER-BUFFER `last_write` event instead of the context-wide
        // singleton `e_h2d` barrier.
        //
        // The old code (AUDIT 2026-05-24, C32) made the compute stream
        // wait on `ctx.barriers.e_h2d`, a single context-shared event
        // that `from_host`/`from_host_async` re-record on EVERY upload
        // (see `upload_via_copy_h2d_stream`, ~line 692). Under concurrent
        // use that singleton races: thread A uploads buffer X and records
        // `e_h2d`, thread B then uploads buffer Y and clobbers `e_h2d`
        // before A's launch issues its `wait_event` — so A's launch waits
        // on Y's upload, not X's, and may run before X's upload completes
        // (reading stale/partial device memory). This is the input-side
        // twin of the already-fixed H10b output-side bug.
        //
        // FIX: mirror `launch_on_stream` (launch.rs §2) — gate the
        // compute stream behind EACH device-ptr arg's own `last_write`
        // event, which was recorded on the stream that actually produced
        // that buffer's contents (its upload, or an earlier kernel). A
        // never-written buffer has `None` and is skipped. Snapshotting the
        // event under the mutex avoids holding the lock across the FFI
        // `wait_event` call.
        let compute_raw = ctx.compute.stream as usize;
        for slot in &last_write_slots {
            let maybe_ev = slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .cloned();
            if let Some(ev) = maybe_ev {
                // Written by an earlier launch on THIS stream: the
                // stream already orders us behind it, so the barrier
                // would buy nothing and cost a driver round trip. Same
                // elision as `Stream::wait_event`; see
                // `EventCuda::recorded_on`.
                if ev.recorded_on_raw() == compute_raw {
                    cratonvm_types::gpu_event_census::note_wait_elided();
                    continue;
                }
                cratonvm_types::gpu_event_census::note_wait_issued();
                // SAFETY: `ev` is an `Arc<Event>` cloned out of the slot
                // and kept alive for this iteration; `cu_event_raw()` only
                // borrows its handle for the FFI call, and `compute.stream`
                // is owned by `ctx`. `cuStreamWaitEvent` records a wait op
                // on the compute stream and returns immediately.
                unsafe {
                    cudarc::driver::result::stream::wait_event(
                        ctx.compute.stream,
                        ev.cu_event_raw(),
                        cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
                    )
                    .map_err(map_err("cuStreamWaitEvent compute←last_write"))?;
                }
            }
        }
        let kernel_done = if needs_d2h_sync && !last_write_slots.is_empty() {
            Some(std::sync::Arc::new(self.make_unrecorded_event(ctx)?))
        } else {
            None
        };
        self.launch_raw_on_stream_inner(&ctx.compute, kernel, cfg, args)?;
        // AUDIT 2026-05-17 (PERF Fix 1): after the launch is submitted,
        // record the post-kernel event so a subsequent D→H copy can wait
        // on it without involving the host. Round-7 PERF Fix 3: gate on
        // `needs_d2h_sync` — callers that know no D→H follows skip the
        // bookkeeping.
        //
        // AUDIT 2026-05-29 (H10b fix): record a *fresh per-buffer* event
        // on `compute` and stamp it into every device-ptr arg's
        // `last_write` slot, so `DeviceBuffer::to_host` /
        // `to_host_async` wait on this buffer's own producing kernel.
        // `e_k` is still recorded for backward compatibility with any
        // external pipeline that may wait on it, but the per-buffer
        // event is the authoritative ordering primitive now.
        if needs_d2h_sync {
            if let Some(kernel_done) = kernel_done {
                if let Err(err) = self.record_compute_event(ctx, &kernel_done) {
                    return self.recover_after_compute_completion_event_failure(
                        ctx,
                        &last_write_slots,
                        err,
                    );
                }
                for slot in &last_write_slots {
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(kernel_done.clone());
                }
            }
            // SAFETY: the context is bound and owns both the live barrier event
            // and compute stream for this synchronous submission.
            unsafe {
                cudarc::driver::result::event::record(ctx.barriers.e_k, ctx.compute.stream)
                    .map_err(map_err("cuEventRecord e_k"))?;
            }
        }
        Ok(())
    }

    /// Create a fresh `crate::Event` before kernel submission so an
    /// event-allocation failure cannot happen after the kernel has
    /// already been queued.
    fn make_unrecorded_event(&self, ctx: &DeviceContextInner) -> Result<crate::Event> {
        // `crate::Event::new` needs a `&DeviceContext`; reconstruct a
        // cheap clone wrapper around this inner context. `DeviceContext`
        // is a newtype over `DeviceContextInner` and `Clone`, so this is
        // an Arc bump, not a new driver context.
        let ctx_pub = crate::DeviceContext::from_inner(ctx.clone());
        crate::Event::new(&ctx_pub)
    }

    /// Record a per-buffer completion event on the compute stream after
    /// a kernel launch has been submitted.
    fn record_compute_event(&self, ctx: &DeviceContextInner, event: &crate::Event) -> Result<()> {
        // BUGFIX 2026-06-17 (cuda-backend SOUND): explicitly bind the
        // primary context to THIS thread before `cuEventRecord`. The
        // `unsafe impl Send + Sync` across the bridge is sound only when
        // the driving thread has bound the device first; `Event::new`
        // happens to bind during creation, but relying on that side
        // effect is fragile — make the invariant local and self-evident
        // at the record site. `bind_to_thread` is a cheap per-thread TLS
        // check (no-op when already bound).
        ctx.bind_to_thread()?;
        // SAFETY: the event is owned by `event` for the rest of this
        // call (and beyond, via the returned value); `compute.stream` is
        // owned by `ctx`. `cuEventRecord` borrows both only for the FFI
        // call. The `bind_to_thread` above ensures this thread drives the
        // correct primary context, satisfying the bridge's Send/Sync
        // contract.
        unsafe {
            cudarc::driver::result::event::record(event.cu_event_raw(), ctx.compute.stream)
                .map_err(map_err("cuEventRecord kernel_done (compute)"))?;
        }
        // Same bookkeeping `Stream::record_event` does, so the wait loop
        // above can recognise its own stream and skip the barrier.
        event.set_recorded_on(ctx.compute.stream);
        Ok(())
    }

    fn recover_after_compute_completion_event_failure(
        &self,
        ctx: &DeviceContextInner,
        last_write_slots: &[crate::LastWriteSlot],
        err: DeviceError,
    ) -> Result<()> {
        if let Err(bind_err) = ctx.bind_to_thread() {
            return Err(DeviceError::Launch(format!(
                "kernel completion event recording failed ({err}); bind_to_thread cleanup failed \
                ({bind_err})"
            )));
        }
        // SAFETY: `ctx` is now bound on this thread and owns the live compute
        // stream until this wait returns.
        match unsafe { cudarc::driver::result::stream::synchronize(ctx.compute.stream) } {
            Ok(()) => {
                for slot in last_write_slots {
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }
                Err(err)
            }
            Err(sync_err) => Err(DeviceError::Launch(format!(
                "kernel completion event recording failed ({err}); compute stream synchronize \
                 cleanup failed ({sync_err:?})"
            ))),
        }
    }

    /// AUDIT 2026-05-24 (C32 stream-port fix): real per-stream launch.
    ///
    /// Used by `DeviceModule::launch_on_stream` to submit a kernel on
    /// a caller-supplied [`crate::Stream`] (not the context's
    /// `compute` stream). The arg-marshalling logic is shared with
    /// the default-stream `launch_raw_inner`; the only difference is
    /// which `CudaStream` `LaunchAsync::launch_on_stream` receives.
    ///
    /// Note: this path does NOT automatically wait on `e_h2d` or
    /// record `e_k`. Callers that need cross-stream ordering with the
    /// context's copy streams are expected to use
    /// `Stream::record_event` / `wait_event` explicitly (see
    /// `event.rs`).
    pub(crate) fn launch_raw_on_stream(
        &self,
        _ctx: &DeviceContextInner,
        stream: &Arc<CudaStream>,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.launch_raw_on_stream_inner(stream, kernel, cfg, args)
    }

    fn launch_raw_on_stream_inner(
        &self,
        stream: &Arc<CudaStream>,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        let func = self
            .functions
            .get(kernel)
            .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;
        let cudarc_cfg = CudarcLaunchConfig {
            grid_dim: cfg.grid,
            block_dim: cfg.block,
            shared_mem_bytes: cfg.shared_bytes,
        };
        // AUDIT 2026-05-17 (PERF Fix 1): launch on the dedicated
        // compute stream. Buffers uploaded via `DeviceBufferInner::
        // from_host` recorded an event on `copy_h2d` and made the
        // compute stream wait on it, so this launch is correctly
        // ordered behind every input upload without forcing the host
        // to block.
        // AUDIT 2026-05-17 (PERF Fix 2): rent the thread-local pointer
        // scratch by `take`-ing it out, refilling it, and putting it
        // back via a drop guard. This avoids holding a `RefMut` across
        // a closure boundary.
        let mut ptr_h = PTR_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        ptr_h.clear();
        // Pre-size in one shot so we don't realloc mid-loop
        let n_ptrs = args
            .raw
            .iter()
            .filter(|a| matches!(a, KernelArg::DevicePtr { .. }))
            .count();
        ptr_h.reserve(n_ptrs);
        for a in &args.raw {
            if let KernelArg::DevicePtr { addr, .. } = a {
                ptr_h.push(*addr);
            }
        }
        // Build the argument tuple for cudarc 0.13 launch API.
        //
        // The CUDA kernel-parameter ABI (and cudarc's `launch_on_stream`)
        // requires each entry to be a pointer TO the argument value, not
        // the value itself. For scalars `&value as *const _` is taken
        // from `args.raw`, which lives for the whole function. For device
        // pointers we must point at a stable 8-byte slot holding the
        // device address: `ptr_h` provides that storage and is kept live
        // (not moved or dropped) until after `launch_on_stream` returns.
        //
        // LIFETIME INVARIANT (enforced below): every pointer in
        // `launch_args` borrows INTO either `args.raw`'s backing store or
        // `ptr_h`'s backing store. Both `args` and `ptr_h` MUST stay live
        // and un-reallocated until `launch_on_stream` has returned. We
        // bind `arg_store` as an explicit `&Vec<KernelArg>` reference so
        // the borrow checker pins `args` for at least the span of that
        // reference, and we add a `_keep_alive` anchor after the launch
        // so any future refactor that drops `args`/`ptr_h` early fails to
        // compile.
        let arg_store: &Vec<KernelArg> = &args.raw;
        let mut ip = 0usize;
        // AUDIT 2026-05-20 (PERF Fix #3): rent the thread-local pointer-vec
        // scratch instead of allocating a fresh `Vec` per launch. Same
        // rent/clear/refill/return-via-drop pattern as `PTR_SCRATCH`.
        let mut launch_args: Vec<*mut std::ffi::c_void> =
            ARG_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        launch_args.clear();
        launch_args.reserve(arg_store.len());
        launch_args.extend(arg_store.iter().map(|a| {
            match a {
                KernelArg::I32(v) => v as *const i32 as *mut std::ffi::c_void,
                KernelArg::I64(v) => v as *const i64 as *mut std::ffi::c_void,
                KernelArg::F32(v) => v as *const f32 as *mut std::ffi::c_void,
                KernelArg::F64(v) => v as *const f64 as *mut std::ffi::c_void,
                KernelArg::DevicePtr { .. } => {
                    // Pointer to the device-address slot in `ptr_h`, not
                    // the device address cast to a pointer.
                    let slot = &ptr_h[ip] as *const u64 as *mut std::ffi::c_void;
                    ip += 1;
                    slot
                }
            }
        }));

        // cudarc 0.13's `LaunchAsync::launch_on_stream` consumes the
        // `CudaFunction` by value (`self`). `CudaFunction` is not
        // `Copy`, and `self.functions` only lends a `&CudaFunction`, so
        // we clone it for the launch — the clone is cheap (it wraps an
        // `Arc<CudaModule>` plus a raw `CUfunction` handle).
        //
        // SAFETY: `launch_on_stream` reads, for every entry of
        // `launch_args`, the bytes the entry points at. Those bytes live
        // in two backing stores that MUST remain allocated, un-moved, and
        // un-reallocated for the full duration of this call:
        //   * `args.raw` (aliased here as `arg_store`) — holds every
        //     scalar `KernelArg` value; the scalar pointers in
        //     `launch_args` point directly at those `Vec` elements.
        //   * `ptr_h` — the thread-local-rented `Vec<u64>` of device-
        //     address slots; the `DevicePtr` pointers in `launch_args`
        //     point at its elements.
        // `launch_on_stream` is synchronous on the host side w.r.t.
        // argument marshalling: it copies the pointed-at parameter bytes
        // into the driver before returning, so the pointers only need to
        // be valid until this call returns (not until the kernel runs).
        // `func.clone()` does not touch either store. Neither `args` nor
        // `ptr_h` is mutated, moved, or reallocated between building
        // `launch_args` and this call. The `_keep_alive` binding after
        // the launch ties both objects' lifetimes past this point so the
        // contract is compiler-enforced against future refactors.
        // AUDIT 2026-05-24 (C32 stream-port fix): launch on the
        // supplied `stream`. Previously this was hard-coded to
        // `&ctx.compute`, which broke `DeviceModule::launch_on_stream`
        // (HIGH-2: user-supplied stream was silently ignored). With
        // this fix, the default-stream caller (`launch_raw_inner`)
        // passes `&ctx.compute` and the explicit-stream caller
        // (`launch_raw_on_stream` / `DeviceModule::launch_on_stream`)
        // passes the user's `Stream`'s inner cudarc `CudaStream`.
        let launch_result = unsafe {
            func.clone()
                .launch_on_stream(stream, cudarc_cfg, &mut launch_args)
                .map_err(map_err("kernel launch"))
        };
        // Liveness anchor: `launch_on_stream` has returned, so the raw
        // pointers in `launch_args` are no longer dereferenced by the
        // driver. Borrowing `args` and `ptr_h` here forces the borrow
        // checker to keep both alive across the `unsafe` launch above —
        // a future refactor that drops/moves either before this point
        // will fail to compile rather than silently introduce UB.
        let _keep_alive: (&KernelArgs, &Vec<u64>) = (&args, &ptr_h);
        // AUDIT 2026-05-20 (PERF Fix #3): return the pointer-vec scratch
        // to its thread-local with capacity intact. The launch has
        // returned, so the driver no longer dereferences these pointers;
        // `_keep_alive` already pinned the backing stores across the
        // launch. Cleared before storing so no dangling pointers linger.
        ARG_SCRATCH.with(|cell| {
            launch_args.clear();
            *cell.borrow_mut() = launch_args;
        });
        // Restore the scratch vec into the thread-local with its
        // (possibly grown) capacity intact, regardless of launch
        // outcome. Safe to consume `ptr_h` now: the launch has returned
        // and `_keep_alive` has already pinned it across the launch.
        PTR_SCRATCH.with(|cell| {
            ptr_h.clear();
            *cell.borrow_mut() = ptr_h;
        });
        launch_result?;
        // AUDIT 2026-05-24 (C32 stream-port fix): post-launch
        // bookkeeping (recording `e_k`, etc.) is now the caller's
        // responsibility — `launch_raw_inner` records `e_k` on the
        // compute stream when `needs_d2h_sync` is set;
        // `launch_raw_on_stream` does not, leaving the user-supplied
        // stream's ordering to explicit `Stream::record_event` /
        // `wait_event` calls.
        Ok(())
    }
}

/// Round-5: H→D upload helper.
///
/// AUDIT 2026-05-24 (C32 stream-port fix): rewritten. Previously this
/// called `ctx.dev.htod_sync_copy(host)`, which submits the H→D memcpy
/// onto cudarc's *default* stream and host-blocks until it completes —
/// every upload thereby (a) ignored the dedicated `copy_h2d` stream
/// the context constructs and (b) serialised the entire pipeline at
/// the host. The new path:
///
///   1. Asynchronously allocates an uninit `CudaSlice<T>` on the
///      device (the allocation itself does not transfer data).
///   2. Issues `cuMemcpyHtoDAsync` against `copy_h2d.stream` so the
///      transfer runs concurrently with any pending compute work.
///   3. Records `e_h2d` on `copy_h2d` so subsequent kernel launches
///      can `cuStreamWaitEvent` on it from the compute stream.
///
/// The caller (`DeviceBufferInner::from_host`) is responsible for
/// host-synchronising before the borrowed `host` slice can be safely
/// dropped or mutated, because `cuMemcpyHtoDAsync` does NOT retain
/// `host` — see the SAFETY paragraph at the call site.
#[inline]
unsafe fn upload_via_copy_h2d_stream<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static>(
    ctx: &DeviceContextInner,
    host: &[T],
) -> Result<CudaSlice<T>> {
    // Allocate uninitialised device storage. cudarc's `alloc` takes
    // `&Arc<CudaDevice>`; it `bind_to_thread`s internally.
    // SAFETY: T is a valid device representation and the returned slice owns
    // exactly `host.len()` uninitialized device elements.
    let slice: CudaSlice<T> = unsafe {
        ctx.dev
            .alloc::<T>(host.len())
            .map_err(map_err("alloc for from_host"))?
    };
    // Submit the H→D copy onto the dedicated upload stream. `device_ptr`
    // on a `&CudaSlice<T>` returns `&CUdeviceptr`; we deref-copy it.
    let dst = *DevicePtr::device_ptr(&slice);
    // SAFETY: allocation and handles share `ctx`; the caller keeps `host`
    // alive until the upload stream completes.
    unsafe {
        cudarc::driver::result::memcpy_htod_async(dst, host, ctx.copy_h2d.stream)
            .map_err(map_err("cuMemcpyHtoDAsync copy_h2d"))?;
        // Record `e_h2d` so the compute stream can `wait_event` on it
        // without host involvement. Re-recording overwrites the prior
        // marker, which is the documented `cuEventRecord` semantic.
        cudarc::driver::result::event::record(ctx.barriers.e_h2d, ctx.copy_h2d.stream)
            .map_err(map_err("cuEventRecord e_h2d"))?;
    }
    Ok(slice)
}

/// AUDIT 2026-05-29 (H10a fix — upload ordered on the USER stream).
///
/// Allocate device storage and submit the H→D copy onto an explicit
/// `upload_stream` (the user's [`crate::Stream`]'s raw handle), then
/// record the buffer's per-buffer `last_write` event on that SAME
/// stream. Returns *without* host-synchronising.
///
/// This is the correctly-async upload used by
/// `DeviceBuffer::from_host_async_unchecked`. Because the DMA and the recorded
/// event both live on the user stream, the documented lifetime
/// contract — "the host slice need only outlive `stream.synchronize()`"
/// — actually holds: a sync on the user stream orders (and thus
/// completes) the DMA. The previous code ran the DMA on the context's
/// `copy_h2d` stream, so a user-stream sync did NOT order the copy and
/// dropping `host` after only that sync was a host-memory UAF.
///
/// SAFETY: `cuMemcpyHtoDAsync` does NOT retain `host` past the call,
/// but the device read from `host` is still in flight when the call
/// returns. The caller (`DeviceBuffer::from_host_async_unchecked`) MUST keep
/// `host` valid and un-moved until `upload_stream` has synchronised
/// (the documented contract). `last_write_event` is recorded on
/// `upload_stream` so any later `cuStreamWaitEvent(other, last_write)`
/// gates on the copy retiring.
#[inline]
unsafe fn upload_on_stream<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static>(
    ctx: &DeviceContextInner,
    host: &[T],
    upload_stream: cudarc::driver::sys::CUstream,
    last_write_event: cudarc::driver::sys::CUevent,
) -> Result<CudaSlice<T>> {
    // SAFETY: T is a valid device representation and the returned slice owns
    // exactly `host.len()` uninitialized device elements.
    let slice: CudaSlice<T> = unsafe {
        ctx.dev
            .alloc::<T>(host.len())
            .map_err(map_err("alloc for from_host_async"))?
    };
    let dst = *DevicePtr::device_ptr(&slice);
    // SAFETY: allocation and handles share a live context; the caller upholds
    // `host` lifetime through completion of `upload_stream`.
    unsafe {
        cudarc::driver::result::memcpy_htod_async(dst, host, upload_stream)
            .map_err(map_err("cuMemcpyHtoDAsync user_stream"))?;
        // Record the buffer's own last_write event on the upload
        // stream so consumers (`launch_on_stream`, `to_host_async`)
        // order behind the copy via the per-buffer event.
        cudarc::driver::result::event::record(last_write_event, upload_stream)
            .map_err(map_err("cuEventRecord last_write (upload)"))?;
    }
    Ok(slice)
}

/// A typed device-side allocation backed by cudarc's safe `CudaSlice<T>`.
/// The buffer also retains the streams it participates in so `to_host`
/// can host-block on the compute stream before issuing the D→H copy.
///
/// AUDIT 2026-05-22 (UAF fix): the `CudaSlice<T>` is held behind an
/// `Arc`. The slice owns the device allocation and frees it on `Drop`;
/// `device_ptr_arg` hands a *clone* of this `Arc` to `KernelArgs` so the
/// device memory is provably kept alive until the launch that reads it
/// has run. Previously `device_ptr_arg` returned only a bare `u64`
/// address with no lifetime tie, so dropping the `DeviceBuffer` before
/// `launch_raw` left the kernel reading freed device memory.
pub(crate) struct DeviceBufferInner<T> {
    slice: Arc<CudaSlice<T>>,
    /// The stream the allocation is bound to. For uploaded buffers this
    /// is `copy_h2d`; for `uninit`/`zeros` it's `compute` (most kernels
    /// write into these output buffers).
    #[allow(dead_code)]
    stream: Arc<CudaStream>,
    /// Retained handle to the cudarc device. Currently unused — kept
    /// so future code that needs a device-bound operation on a buffer
    /// (e.g. `bind_to_thread`) doesn't have to re-thread the device.
    #[allow(dead_code)]
    dev: Arc<CudaDevice>,
    /// D→H copy stream, used by `to_host`.
    copy_d2h: Arc<CudaStream>,
    /// Compute stream — the stream every kernel launch runs on.
    /// `to_host` host-blocks on this before reading the buffer back so
    /// the D→H copy is correctly ordered after the kernel regardless of
    /// which `launch_raw` variant was used.
    #[allow(dead_code)]
    compute: Arc<CudaStream>,
    /// AUDIT 2026-05-24 (C32 stream-port fix): retained barrier events
    /// from the owning `DeviceContextInner`.
    ///
    /// AUDIT 2026-05-29 (H10b fix — per-buffer events): `to_host` /
    /// `to_host_async` no longer wait on the context-wide `e_k`; they
    /// wait on the buffer's own `last_write` event (threaded in from
    /// `lib.rs`). This handle is now retained purely to keep the
    /// context's barrier events (used by the legacy default-stream
    /// `launch_raw_inner` path) alive for as long as any buffer that
    /// participated in that pipeline lives. It is no longer read by the
    /// buffer's D→H path.
    #[allow(dead_code)]
    _barriers: Arc<StreamBarriers>,
}

/// Reject element counts whose byte size overflows `usize` before
/// handing `len` to the driver, which would otherwise allocate a
/// wrapped (too-small) buffer (or wrap inside cudarc).
///
/// AUDIT 2026-05-20 (PERF Fix #4): shared guard for `uninit` and
/// `zeros` — `zeros` previously called `alloc_zeros::<T>(len)` with no
/// overflow check at all.
#[inline]
fn check_alloc_size<T>(stage: &str, len: usize) -> Result<()> {
    len.checked_mul(std::mem::size_of::<T>()).ok_or_else(|| {
        DeviceError::Driver(format!(
            "{stage}: size overflow ({len} elements of {} bytes)",
            std::mem::size_of::<T>()
        ))
    })?;
    Ok(())
}

impl<
        T: bytemuck::Pod
            + DeviceRepr
            + Send
            + Sync
            + 'static
            + cudarc::driver::ValidAsZeroBits
            + std::marker::Unpin,
    > DeviceBufferInner<T>
{
    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        check_alloc_size::<T>("alloc uninit", len)?;
        // Output buffers are bound to the compute stream — the kernel
        // launch that fills them already runs there.
        // SAFETY: overflow was rejected and T is a valid device
        // representation; CudaSlice owns the allocation.
        let slice = unsafe { ctx.dev.alloc::<T>(len).map_err(map_err("alloc uninit"))? };
        Ok(Self {
            slice: Arc::new(slice),
            stream: ctx.compute.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
            compute: ctx.compute.clone(),
            _barriers: ctx.barriers.clone(),
        })
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        // AUDIT 2026-05-20 (PERF Fix #4): same overflow guard as `uninit`
        // — `alloc_zeros` would otherwise wrap inside cudarc on a huge `len`.
        check_alloc_size::<T>("alloc_zeros", len)?;
        let slice = ctx
            .dev
            .alloc_zeros::<T>(len)
            .map_err(map_err("alloc_zeros"))?;
        Ok(Self {
            slice: Arc::new(slice),
            stream: ctx.compute.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
            compute: ctx.compute.clone(),
            _barriers: ctx.barriers.clone(),
        })
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the primary context to
        // this thread before any raw FFI (`memcpy_htod_async`,
        // `cuStreamSynchronize`). cudarc's `alloc` binds internally too,
        // but we bind explicitly so the contract is visible at the
        // transfer entry point.
        ctx.bind_to_thread()?;
        // AUDIT 2026-05-17 (PERF Fix 1): H→D upload runs on the
        // dedicated copy_h2d stream, then records an event the compute
        // stream waits on. This lets the next host-side launch_raw
        // schedule a kernel without blocking on the upload to complete.
        //
        // AUDIT 2026-05-24 (C32 stream-port fix): previously this called
        // `upload_via_pinned_or_fallback` → `htod_sync_copy`, which runs
        // on cudarc's *default* stream and host-blocks until the copy
        // completes. The `ctx.dev.wait_for(&ctx.copy_h2d)` below then
        // waited on an empty stream (no-op), and the buffer's
        // `stream = copy_h2d` field lied about where the copy ran. The
        // three-stream pipeline bought nothing in the H→D direction.
        //
        // The new helper submits `cuMemcpyHtoDAsync` against
        // `copy_h2d.stream` and records `e_h2d` so the compute stream
        // can `cuStreamWaitEvent` on it without host involvement.
        //
        // SAFETY: `memcpy_htod_async` is asynchronous w.r.t. the host —
        // it does NOT retain `host` past the call but the device read
        // from `host` is still in flight when the call returns. The
        // synchronous `from_host` API contract requires the upload to
        // be observable when this function returns (callers may drop
        // or mutate `host` immediately after), so we host-block on the
        // `copy_h2d` stream below. The new `from_host_async` path
        // (on `lib.rs`, gated on the `cuda` feature) bypasses this
        // host-block — it documents that the borrowed `host` slice
        // must outlive the stream synchronisation point.
        let slice = unsafe { upload_via_copy_h2d_stream(ctx, host) }?;
        // Host-block on `copy_h2d` to preserve the sync `from_host`
        // contract. Uses `cuStreamSynchronize` rather than
        // `cuCtxSynchronize` so the compute and copy_d2h streams keep
        // running concurrently with whatever the caller does next.
        // SAFETY: the context is bound and owns the live upload stream; this
        // wait discharges the borrowed-host lifetime obligation.
        unsafe {
            cudarc::driver::result::stream::synchronize(ctx.copy_h2d.stream)
                .map_err(map_err("cuStreamSynchronize copy_h2d"))?;
        }
        Ok(Self {
            slice: Arc::new(slice),
            stream: ctx.copy_h2d.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
            compute: ctx.compute.clone(),
            _barriers: ctx.barriers.clone(),
        })
    }

    /// Async upload variant.
    ///
    /// AUDIT 2026-05-29 (H10a fix): submits the H→D copy onto the
    /// caller-supplied `upload_stream` (the user's [`crate::Stream`])
    /// and records the buffer's per-buffer `last_write_event` on that
    /// same stream, then returns *without* host-synchronising. The
    /// caller (`DeviceBuffer::from_host_async_unchecked`) MUST keep `host` alive —
    /// and not move/mutate it — until `upload_stream` has synchronised.
    ///
    /// Previously this routed through `upload_via_copy_h2d_stream`,
    /// which ran the DMA on the context's `copy_h2d` stream while the
    /// public contract promised the host slice need only outlive the
    /// USER stream's sync point. A user-stream sync did not order a
    /// copy on `copy_h2d`, so the driver could still be DMA-reading
    /// freed host memory — a latent host-buffer use-after-free. Routing
    /// the copy onto `upload_stream` makes the documented contract
    /// sound.
    pub(crate) unsafe fn from_host_async_unchecked(
        ctx: &DeviceContextInner,
        host: &[T],
        upload_stream: cudarc::driver::sys::CUstream,
        last_write_event: cudarc::driver::sys::CUevent,
    ) -> Result<Self> {
        // SAFETY: the caller (`DeviceBuffer::from_host_async_unchecked`) is
        // responsible for the host-buffer lifetime; see this method's
        // doc comment. `last_write_event` is owned by the caller's
        // `Arc<Event>` and only borrowed for the FFI call.
        let slice = unsafe { upload_on_stream(ctx, host, upload_stream, last_write_event) }?;
        Ok(Self {
            slice: Arc::new(slice),
            // The buffer's contents were produced on `upload_stream`;
            // record that as its bound stream for bookkeeping parity
            // with the sync `from_host` (which binds `copy_h2d`).
            stream: ctx.copy_h2d.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
            compute: ctx.compute.clone(),
            _barriers: ctx.barriers.clone(),
        })
    }

    pub(crate) fn to_host(
        &self,
        dst: &mut [T],
        wait_event: Option<cudarc::driver::sys::CUevent>,
    ) -> Result<()> {
        if dst.len() != self.len() {
            return Err(DeviceError::Memcpy(format!(
                "to_host length mismatch: dst.len()={}, slice.len()={}",
                dst.len(),
                self.len()
            )));
        }
        // AUDIT 2026-05-24 (C32 stream-port fix): previous code called
        // `self.dev.synchronize()` (`cuCtxSynchronize`) and then
        // `dtoh_sync_copy_into`, which is a default-stream synchronous
        // copy. That defeated every stream-overlap claim — every
        // `to_host` drained every other stream on the context.
        //
        // AUDIT 2026-05-29 (H10b fix — per-buffer events): previously
        // this made `copy_d2h` wait on the *context-wide* singleton
        // `e_k` event, which is re-recorded on every launch on ANY
        // buffer. A concurrent pipeline's launch could overwrite `e_k`
        // between this buffer's producing kernel and this read, so the
        // D→H copy could be released before its producer finished and
        // read stale device memory. Now the caller (`lib.rs`) passes
        // THIS buffer's own `last_write` event, recorded on the stream
        // its producing kernel actually ran on.
        //
        // The path:
        //   1. Make `copy_d2h` wait on the buffer's `last_write` event
        //      (if any). If the buffer was never written by a kernel /
        //      upload, there is no event and the wait is skipped.
        //   2. Issue `cuMemcpyDtoHAsync` on `copy_d2h.stream`.
        //   3. Host-block on `copy_d2h.stream` only (via
        //      `cuStreamSynchronize`) — `compute` and `copy_h2d` keep
        //      running concurrently with the caller's next move.
        //
        // SAFETY: `cuMemcpyDtoHAsync` is asynchronous w.r.t. the host,
        // so `dst` is written to *after* the call returns. The
        // synchronous `to_host` contract requires the data to be
        // present in `dst` when this function returns, hence the
        // `cuStreamSynchronize` below. The `_async_raw` variant
        // (used by `DeviceBuffer::to_host_async`) skips this wait and
        // pushes the responsibility onto the caller's stream
        // synchronisation point.
        let src = *DevicePtr::device_ptr(&*self.slice);
        // SAFETY: dst length matches the device slice; all handles remain live
        // and the final stream sync completes writes before dst is reused.
        unsafe {
            if let Some(ev) = wait_event {
                cudarc::driver::result::stream::wait_event(
                    self.copy_d2h.stream,
                    ev,
                    cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
                )
                .map_err(map_err("cuStreamWaitEvent copy_d2h←last_write"))?;
            }
            cudarc::driver::result::memcpy_dtoh_async(dst, src, self.copy_d2h.stream)
                .map_err(map_err("cuMemcpyDtoHAsync copy_d2h"))?;
            cudarc::driver::result::stream::synchronize(self.copy_d2h.stream)
                .map_err(map_err("cuStreamSynchronize copy_d2h"))?;
        }
        Ok(())
    }

    /// Overwrite this buffer's contents in place from `host`.
    ///
    /// The one thing `from_host` cannot do: it allocates, and a caller
    /// that needs the DEVICE POINTER to stay the same cannot allocate.
    /// A captured CUDA graph bakes every argument pointer into its
    /// nodes, so the only way to feed a replay new input is to write
    /// through the pointer it already holds.
    ///
    /// Synchronous, like `from_host` and `to_host`: the copy is
    /// observable on the device when this returns, so a caller may
    /// reuse `host` immediately and a replay submitted afterwards sees
    /// the new bytes. The copy runs on `copy_h2d` and host-blocks on
    /// that stream alone, leaving `compute` and `copy_d2h` running.
    ///
    /// Length must match exactly. A shorter `host` would leave a
    /// partially-updated buffer, which is a wrong answer rather than an
    /// error, and a longer one would write past the allocation.
    pub(crate) fn copy_from_host(&self, host: &[T]) -> Result<()> {
        if host.len() != self.len() {
            return Err(DeviceError::Memcpy(format!(
                "copy_from_host length mismatch: host.len()={}, slice.len()={}",
                host.len(),
                self.len()
            )));
        }
        if host.is_empty() {
            return Ok(());
        }
        self.dev.bind_to_thread().map_err(map_err("bind_to_thread"))?;
        let dst = *DevicePtr::device_ptr(&*self.slice);
        // SAFETY: lengths were checked equal above, the context is bound,
        // and the `cuStreamSynchronize` below discharges the borrow of
        // `host` that the async copy takes.
        unsafe {
            cudarc::driver::result::memcpy_htod_async(dst, host, self.copy_h2d_stream())
                .map_err(map_err("cuMemcpyHtoDAsync copy_from_host"))?;
            cudarc::driver::result::stream::synchronize(self.copy_h2d_stream())
                .map_err(map_err("cuStreamSynchronize copy_from_host"))?;
        }
        Ok(())
    }

    /// The stream `copy_from_host` uploads on -- the same one the
    /// allocation was bound to when it came from `from_host`.
    fn copy_h2d_stream(&self) -> cudarc::driver::sys::CUstream {
        self.stream.stream
    }

    /// AUDIT 2026-05-24 (C32 stream-port fix): truly async D→H.
    ///
    /// Submits `cuMemcpyDtoHAsync` onto the caller-supplied
    /// `user_stream` after making it wait on this buffer's `last_write`
    /// event (so the copy is ordered after the kernel / upload that
    /// produced the buffer's contents). Does NOT host-block — the
    /// caller MUST `user_stream.synchronize()` (or `wait_event` on a
    /// recorded event) before reading `dst`.
    ///
    /// `dst` must outlive the caller's stream-synchronisation point,
    /// because the driver writes to it asynchronously.
    ///
    /// AUDIT 2026-05-29 (H10b fix — per-buffer events): the wait event
    /// is now THIS buffer's `last_write`, passed in by `lib.rs`, rather
    /// than the context-wide singleton `e_k`. `e_k` was clobbered by
    /// every launch on any buffer, so a D→H copy could be released
    /// before its own producing kernel retired. The per-buffer event
    /// is recorded on the stream the producing kernel actually ran on,
    /// so the dependency is exact.
    pub(crate) fn to_host_async_raw(
        &self,
        dst: &mut [T],
        user_stream: cudarc::driver::sys::CUstream,
        wait_event: Option<cudarc::driver::sys::CUevent>,
    ) -> Result<()> {
        if dst.len() != self.len() {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async length mismatch: dst.len()={}, slice.len()={}",
                dst.len(),
                self.len()
            )));
        }
        let src = *DevicePtr::device_ptr(&*self.slice);
        // SAFETY: `wait_event` (if any) is owned by the buffer's
        // `last_write` Arc<Event> kept alive by the `lib.rs` caller for
        // the duration of this call; `cuStreamWaitEvent` only borrows
        // the handle for the FFI call. A never-recorded event would be
        // a no-op, but we additionally skip the wait entirely when the
        // buffer has no recorded write.
        unsafe {
            if let Some(ev) = wait_event {
                cudarc::driver::result::stream::wait_event(
                    user_stream,
                    ev,
                    cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
                )
                .map_err(map_err("cuStreamWaitEvent user_stream←last_write"))?;
            }
            cudarc::driver::result::memcpy_dtoh_async(dst, src, user_stream)
                .map_err(map_err("cuMemcpyDtoHAsync user_stream"))?;
        }
        Ok(())
    }

    /// Async device->host copy of a SUB-RANGE, starting at element
    /// `offset` and running for `dst.len()` elements.
    ///
    /// The whole-buffer [`to_host_async_raw`](Self::to_host_async_raw)
    /// cannot express a chunked writeback: overlapping chunk N's copy with
    /// chunk N+1's kernel needs each copy to name its own slice of one
    /// device buffer. Same `last_write` wait discipline as the full-buffer
    /// form — the wait is on the buffer, because a chunked launch's writes
    /// to *this* slice are ordered by the caller's own per-chunk event.
    pub(crate) fn to_host_async_range_raw(
        &self,
        dst: &mut [T],
        offset: usize,
        user_stream: cudarc::driver::sys::CUstream,
        wait_event: Option<cudarc::driver::sys::CUevent>,
    ) -> Result<()> {
        let end = offset.checked_add(dst.len()).ok_or_else(|| {
            DeviceError::Memcpy("to_host_async_range: offset + len overflows".into())
        })?;
        if end > self.len() {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async_range out of bounds: offset={offset} len={} buffer={}",
                dst.len(),
                self.len()
            )));
        }
        let base = *DevicePtr::device_ptr(&*self.slice);
        // Cast: element offset -> byte offset on the device pointer.
        let src = base + (offset * std::mem::size_of::<T>()) as u64;
        // SAFETY: the range is bounds-checked against the buffer above, so
        // `src .. src + dst.len()*size_of::<T>()` lies inside the
        // allocation. `wait_event` is kept alive by the `lib.rs` caller for
        // the duration of this call, exactly as in `to_host_async_raw`.
        unsafe {
            if let Some(ev) = wait_event {
                cudarc::driver::result::stream::wait_event(
                    user_stream,
                    ev,
                    cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
                )
                .map_err(map_err("cuStreamWaitEvent user_stream<-last_write"))?;
            }
            cudarc::driver::result::memcpy_dtoh_async(dst, src, user_stream)
                .map_err(map_err("cuMemcpyDtoHAsync range user_stream"))?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        DeviceSlice::len(&*self.slice)
    }

    /// AUDIT 2026-05-29 (SOUND-1 / H10c): bind the buffer's owning
    /// primary context to the calling thread. The buffer retains an
    /// `Arc<CudaDevice>` precisely so its cross-thread-callable public
    /// methods (`to_host` / `to_host_async`) can satisfy the
    /// `bind_to_thread` contract without threading a `DeviceContext`
    /// reference through. Cheap per-thread TLS check.
    pub(crate) fn bind_to_thread(&self) -> Result<()> {
        self.dev.bind_to_thread().map_err(map_err("bind_to_thread"))
    }
}

/// Type-erased keep-alive handle for a device allocation.
///
/// AUDIT 2026-05-22 (UAF fix): `device_ptr_arg` returns one of these
/// alongside the raw device address. It is a clone of the buffer's
/// `Arc<CudaSlice<T>>` erased to `dyn Any`, so a `KernelArg::DevicePtr`
/// can hold it without naming `T`. As long as the `KernelArg` (and thus
/// the owning `KernelArgs`) is alive, the underlying `CudaSlice` is not
/// dropped, so the device allocation the kernel reads stays valid even
/// if the original `DeviceBuffer` is dropped before the launch runs.
pub(crate) type BufferKeepAlive = Arc<dyn std::any::Any + Send + Sync + 'static>;

// `device_ptr_arg` needs `DevicePtr<T>` (which cudarc implements for
// `CudaSlice<T>` for *every* `T`) plus `T: Send + Sync + 'static` so the
// `Arc<CudaSlice<T>>` keep-alive can be erased to `Arc<dyn Any + Send +
// Sync>`. `Send + Sync + 'static` is satisfied by every type that can
// back a `DeviceBuffer` (`DeviceElem` already requires it), so this is
// not a real restriction on callers; the heavyweight `Pod + DeviceRepr +
// ValidAsZeroBits + …` allocation bounds still do not leak here.
impl<T: Send + Sync + 'static> DeviceBufferInner<T> {
    /// Return the raw device address together with a type-erased
    /// keep-alive handle to the owning `CudaSlice`.
    ///
    /// AUDIT 2026-05-16 (CRIT-1 fix): In cudarc 0.13, SyncRecord was removed.
    /// Stream ordering is now handled via CudaDevice::wait_for and
    /// fork_default_stream, which we use in from_host and launch_raw.
    ///
    /// AUDIT 2026-05-22 (UAF fix): also returns `BufferKeepAlive`. The
    /// caller (`KernelArgs::push_device_ptr`) stores this in the
    /// `KernelArg::DevicePtr`, so the device allocation behind `addr`
    /// cannot be freed before the launch that consumes the `KernelArgs`.
    pub(crate) fn device_ptr_arg(&self) -> (u64, BufferKeepAlive) {
        let _ = (&self.dev, &self.copy_d2h); // retained for future use
        let addr = *DevicePtr::device_ptr(&*self.slice);
        let keep_alive: BufferKeepAlive = self.slice.clone();
        (addr, keep_alive)
    }
}

/// Page-locked host allocation backing [`crate::PinnedHostBuffer`].
pub(crate) struct PinnedHostInner<T: Copy> {
    ptr: *mut T,
    len: usize,
    _ctx: DeviceContextInner,
    _marker: std::marker::PhantomData<T>,
}

// SAFETY: the allocation is a plain page-locked host region owned by this
// value; the raw pointer is only dereferenced through `as_mut_slice`, whose
// safety contract puts DMA ordering on the caller. Same argument the rest of
// this crate makes for its `CUstream`/`CUevent` handles.
unsafe impl<T: Copy + Send> Send for PinnedHostInner<T> {}
// SAFETY: as above; shared references hand out no interior mutability of
// their own.
unsafe impl<T: Copy + Sync> Sync for PinnedHostInner<T> {}

impl<T: Copy + Default> PinnedHostInner<T> {
    pub(crate) fn new(ctx: &crate::DeviceContext, len: usize) -> Result<Self> {
        let inner = ctx.inner().clone();
        inner.bind_to_thread()?;
        let bytes = len
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("pinned host alloc size overflows".into()))?;
        let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `raw` is a live out-param for the duration of the call and
        // the context is bound on this thread just above.
        unsafe {
            cudarc::driver::sys::lib()
                .cuMemAllocHost_v2(&mut raw, bytes.max(1))
                .result()
                .map_err(map_err("cuMemAllocHost"))?;
        }
        Ok(Self {
            ptr: raw as *mut T,
            len,
            _ctx: inner,
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// # Safety
    /// See [`crate::PinnedHostBuffer::as_mut_slice`].
    pub(crate) unsafe fn as_mut_slice(&self) -> &mut [T] {
        std::slice::from_raw_parts_mut(self.ptr, self.len)
    }
}

impl<T: Copy> Drop for PinnedHostInner<T> {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // Binding can only fail if the context is already gone, in which
        // case the allocation went with it.
        if self._ctx.bind_to_thread().is_err() {
            return;
        }
        // SAFETY: `ptr` came from `cuMemAllocHost_v2` in `new` and is freed
        // exactly once, here.
        unsafe {
            let _ = cudarc::driver::sys::lib().cuMemFreeHost(self.ptr as *mut std::ffi::c_void);
        }
    }
}
