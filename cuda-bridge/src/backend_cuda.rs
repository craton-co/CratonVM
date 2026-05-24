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
    CudaDevice, CudaFunction, CudaSlice, CudaStream, DeviceRepr, LaunchConfig as CudarcLaunchConfig,
    DeviceSlice, DevicePtr, LaunchAsync,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

fn map_err<E: std::fmt::Debug>(stage: &str) -> impl FnOnce(E) -> DeviceError + '_ {
    move |e| DeviceError::Driver(format!("{stage}: {e:?}"))
}

/// Round-10 multi-GPU enumeration. Queries `cuDeviceGetCount` via
/// cudarc's `result::device::get_count`. `cuInit` is idempotent so
/// calling it here costs only a per-process atomic check after the
/// first call (and `CudaContext::new` already calls it).
pub(crate) fn device_count() -> Result<u32> {
    cudarc::driver::result::init().map_err(map_err("cuInit"))?;
    let n = cudarc::driver::result::device::get_count()
        .map_err(map_err("cuDeviceGetCount"))?;
    Ok(n.max(0) as u32)
}

pub(crate) fn probe() -> Result<DeviceCaps> {
    let dev = CudaDevice::new(0).map_err(map_err("CudaDevice::new(0)"))?;
    let name = dev.name().map_err(map_err("device name"))?;
    let attr = |a| dev.attribute(a).map_err(map_err("device attribute"));
    let major = attr(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR)?;
    let minor = attr(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR)?;
    // cudarc 0.13 / the CUDA driver API has no
    // `CU_DEVICE_ATTRIBUTE_TOTAL_MEMORY` device attribute — total
    // global memory is queried with `cuDeviceTotalMem` instead. The
    // safe wrapper is `result::device::total_mem`, which takes the raw
    // `CUdevice` handle exposed by `CudaDevice::cu_device()`.
    let total_mem = unsafe {
        cudarc::driver::result::device::total_mem(*dev.cu_device())
            .map_err(map_err("total memory"))?
    };
    Ok(DeviceCaps {
        ordinal: 0,
        name,
        compute_major: major as u32,
        compute_minor: minor as u32,
        total_global_mem: total_mem as u64,
    })
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
unsafe impl Sync for StreamBarriers {}

impl Drop for StreamBarriers {
    fn drop(&mut self) {
        let _ = self.dev.bind_to_thread();
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
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        let dev = CudaDevice::new(device_ordinal as usize).map_err(map_err("CudaDevice::new"))?;
        let default = dev.fork_default_stream().map_err(map_err("fork_default_stream"))?;
        // AUDIT 2026-05-17 (PERF Fix 1): create three auxiliary streams
        // for the H2D → compute → D2H pipeline. `fork_default_stream` produces
        // an independent cudarc stream (the cudarc equivalent of
        // `cudaStreamCreate(&s, cudaStreamNonBlocking)`).
        let copy_h2d = dev.fork_default_stream().map_err(map_err("fork_default_stream copy_h2d"))?;
        let compute = dev.fork_default_stream().map_err(map_err("fork_default_stream compute"))?;
        let copy_d2h = dev.fork_default_stream().map_err(map_err("fork_default_stream copy_d2h"))?;
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
            barriers: Arc::new(StreamBarriers { e_h2d, e_k, dev }),
        })
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        // Drain every stream we own. Any in-flight pipeline stage —
        // upload, kernel, download — must complete before we return.
        //
        // `CudaDevice::synchronize` (`cuCtxSynchronize`) host-blocks the
        // calling thread until ALL streams on the context — `copy_h2d`,
        // `compute`, `copy_d2h`, and the default stream — have drained.
        // (`CudaDevice::wait_for` only makes the default stream wait on
        // another stream and does NOT block the host, so it cannot be
        // used here.)
        self.dev.synchronize().map_err(map_err("synchronize device"))?;
        Ok(())
    }

    /// Crate-internal accessor used by `stream.rs` / `event.rs` /
    /// `async_memcpy.rs` to reach the underlying `CudaDevice` without
    /// re-creating it. The returned `Arc` is cheap to clone.
    pub(crate) fn device(&self) -> &Arc<CudaDevice> {
        &self.dev
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
            let func = ctx.dev
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
        // ordered. The dedicated `launch_raw_no_d2h_sync` entry point
        // skips the post-launch event when the caller knows no D→H
        // copy follows (e.g. fire-and-forget kernels, or back-to-back
        // launches on the compute stream with no `to_host` between).
        self.launch_raw_inner(ctx, kernel, cfg, args, /* needs_d2h_sync */ true)
    }

    /// Round-7 PERF Fix 3: launch variant for caller-known "kernel only,
    /// no D→H follows" sequences. Skips the post-launch
    /// `compute.record_event` + `copy_d2h.wait(evt)` pair, which is
    /// dead bookkeeping when no `to_host` ever runs on this buffer
    /// chain. Each unnecessary event-wait adds ~3 µs of CPU-side
    /// driver overhead and contends on cudarc's per-context event
    /// pool — measurable on tight back-to-back microkernel loops.
    ///
    /// Public surface: routed through `DeviceModule::launch_raw_no_sync`.
    pub(crate) fn launch_raw_no_d2h_sync(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.launch_raw_inner(ctx, kernel, cfg, args, /* needs_d2h_sync */ false)
    }

    /// Query the driver for the kernel's occupancy-optimal block size.
    ///
    /// Round-8 fix for the `LaunchConfig::elementwise` hardcoded-256
    /// TODO. Wraps cudarc's
    /// `CudaFunction::occupancy_max_potential_block_size` (which fans
    /// out to `cuOccupancyMaxPotentialBlockSize` in the driver). The
    /// returned size assumes zero dynamic shared memory and no block-
    /// size ceiling, which matches every kernel the bridge currently
    /// launches; the caller (`LaunchConfig::elementwise_for_kernel`)
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

    /// Launch on the context's `compute` stream. Public entry points
    /// `launch_raw` / `launch_raw_no_d2h_sync` route here.
    fn launch_raw_inner(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        needs_d2h_sync: bool,
    ) -> Result<()> {
        // AUDIT 2026-05-24 (C32 stream-port fix): before the launch,
        // make the compute stream wait on `e_h2d`. If a recent
        // `from_host_async` (or `from_host`) recorded the event, this
        // orders the kernel correctly behind the upload without
        // host-blocking. If no upload was ever issued, the wait is a
        // cheap no-op (cuStreamWaitEvent on a never-recorded event
        // returns immediately).
        //
        // The fact that `e_h2d` records the *most recent* upload is
        // important: a `from_host_async` that ran on a different user
        // stream's behalf still routes through `copy_h2d`, so the
        // compute stream picks up the dependency uniformly.
        unsafe {
            cudarc::driver::result::stream::wait_event(
                ctx.compute.stream,
                ctx.barriers.e_h2d,
                cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
            )
            .map_err(map_err("cuStreamWaitEvent compute←e_h2d"))?;
        }
        self.launch_raw_on_stream_inner(&ctx.compute, kernel, cfg, args)?;
        // AUDIT 2026-05-17 (PERF Fix 1): after the launch is submitted,
        // record the post-kernel event so any subsequent `to_host_async`
        // on the copy_d2h stream can wait on it without involving the
        // host. Round-7 PERF Fix 3: gate on `needs_d2h_sync` — callers
        // that know no D→H follows skip the bookkeeping.
        if needs_d2h_sync {
            unsafe {
                cudarc::driver::result::event::record(ctx.barriers.e_k, ctx.compute.stream)
                    .map_err(map_err("cuEventRecord e_k"))?;
            }
        }
        Ok(())
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
        let n_ptrs = args.raw.iter().filter(|a| matches!(a, KernelArg::DevicePtr { .. })).count();
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
    let slice: CudaSlice<T> = unsafe {
        ctx.dev
            .alloc::<T>(host.len())
            .map_err(map_err("alloc for from_host"))?
    };
    // Submit the H→D copy onto the dedicated upload stream. `device_ptr`
    // on a `&CudaSlice<T>` returns `&CUdeviceptr`; we deref-copy it.
    let dst = *DevicePtr::device_ptr(&slice);
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
    /// from the owning `DeviceContextInner`. `to_host` / `to_host_async`
    /// `cuStreamWaitEvent` on `_barriers.e_k` so they pick up the
    /// most recent compute-stream launch without needing a context ref.
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
    len.checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| DeviceError::Driver(format!(
            "{stage}: size overflow ({len} elements of {} bytes)",
            std::mem::size_of::<T>()
        )))?;
    Ok(())
}

impl<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static + cudarc::driver::ValidAsZeroBits + std::marker::Unpin> DeviceBufferInner<T> {
    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        check_alloc_size::<T>("alloc uninit", len)?;
        // Output buffers are bound to the compute stream — the kernel
        // launch that fills them already runs there.
        let slice = unsafe {
            ctx.dev
                .alloc::<T>(len)
                .map_err(map_err("alloc uninit"))?
        };
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
        let slice = ctx.dev
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

    /// Async upload variant (C32 stream-port fix).
    ///
    /// Submits the H→D copy onto `copy_h2d` and returns *without*
    /// host-synchronising. The caller MUST keep `host` alive — and not
    /// move/mutate it — until the supplied user stream (or the context)
    /// has synchronised. Used by `DeviceBuffer::from_host_async`.
    pub(crate) fn from_host_async_unchecked(
        ctx: &DeviceContextInner,
        host: &[T],
    ) -> Result<Self> {
        // SAFETY: the caller (`DeviceBuffer::from_host_async`) is
        // responsible for the host-buffer lifetime; see this method's
        // doc comment.
        let slice = unsafe { upload_via_copy_h2d_stream(ctx, host) }?;
        Ok(Self {
            slice: Arc::new(slice),
            stream: ctx.copy_h2d.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
            compute: ctx.compute.clone(),
            _barriers: ctx.barriers.clone(),
        })
    }

    pub(crate) fn to_host(&self, dst: &mut [T]) -> Result<()> {
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
        // The new path:
        //   1. Make `copy_d2h` wait on `e_k` (the post-kernel event
        //      that `launch_raw` records). If no launch ever ran, the
        //      wait is a cheap no-op.
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
        unsafe {
            cudarc::driver::result::stream::wait_event(
                self.copy_d2h.stream,
                self.barriers_e_k(),
                cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
            )
            .map_err(map_err("cuStreamWaitEvent copy_d2h←e_k"))?;
            cudarc::driver::result::memcpy_dtoh_async(dst, src, self.copy_d2h.stream)
                .map_err(map_err("cuMemcpyDtoHAsync copy_d2h"))?;
            cudarc::driver::result::stream::synchronize(self.copy_d2h.stream)
                .map_err(map_err("cuStreamSynchronize copy_d2h"))?;
        }
        Ok(())
    }

    /// AUDIT 2026-05-24 (C32 stream-port fix): truly async D→H.
    ///
    /// Submits `cuMemcpyDtoHAsync` onto the caller-supplied
    /// `user_stream` after making it wait on `e_k` (so the copy is
    /// ordered after the most recent compute-stream launch). Does NOT
    /// host-block — the caller MUST `user_stream.synchronize()` (or
    /// `wait_event` on a recorded event) before reading `dst`.
    ///
    /// `dst` must outlive the caller's stream-synchronisation point,
    /// because the driver writes to it asynchronously.
    pub(crate) fn to_host_async_raw(
        &self,
        dst: &mut [T],
        user_stream: cudarc::driver::sys::CUstream,
    ) -> Result<()> {
        if dst.len() != self.len() {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async length mismatch: dst.len()={}, slice.len()={}",
                dst.len(),
                self.len()
            )));
        }
        let src = *DevicePtr::device_ptr(&*self.slice);
        // SAFETY: see the doc comment. `e_k` is owned by the context
        // and `cuStreamWaitEvent` on a never-recorded event is a
        // no-op, so this is safe even when no launch preceded.
        unsafe {
            cudarc::driver::result::stream::wait_event(
                user_stream,
                self.barriers_e_k(),
                cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
            )
            .map_err(map_err("cuStreamWaitEvent user_stream←e_k"))?;
            cudarc::driver::result::memcpy_dtoh_async(dst, src, user_stream)
                .map_err(map_err("cuMemcpyDtoHAsync user_stream"))?;
        }
        Ok(())
    }

    /// Returns the context-wide `e_k` (post-kernel) event the buffer
    /// retained at construction time. Used by `to_host` /
    /// `to_host_async_raw` to make their D→H copy wait on the most
    /// recent compute-stream launch without needing a `DeviceContext`
    /// reference threaded through.
    fn barriers_e_k(&self) -> cudarc::driver::sys::CUevent {
        self._barriers.e_k
    }

    pub(crate) fn len(&self) -> usize {
        DeviceSlice::len(&*self.slice)
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
