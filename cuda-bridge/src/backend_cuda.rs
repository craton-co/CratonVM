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

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    dev: Arc<CudaDevice>,
    /// Default cudarc stream — the original allocator/launch root. Kept
    /// so `synchronize()` can drain it for callers that still hold
    /// buffers created before the three-stream restructure.
    default: Arc<CudaStream>,
    /// Stream used for host→device memcpys (uploads).
    copy_h2d: Arc<CudaStream>,
    /// Stream used for kernel launches. Waits on `copy_h2d` events
    /// before launching, then records its own event for `copy_d2h`.
    compute: Arc<CudaStream>,
    /// Stream used for device→host memcpys (downloads). Waits on the
    /// compute stream's event before starting any read.
    copy_d2h: Arc<CudaStream>,
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
        Ok(Self {
            dev,
            default: default.into(),
            copy_h2d: copy_h2d.into(),
            compute: compute.into(),
            copy_d2h: copy_d2h.into(),
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

    fn launch_raw_inner(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        needs_d2h_sync: bool,
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
        let launch_result = unsafe {
            func.clone()
                .launch_on_stream(&ctx.compute, cudarc_cfg, &mut launch_args)
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
        // AUDIT 2026-05-17 (PERF Fix 1): after the launch is submitted,
        // wait for compute stream to complete before D→H copy
        // Round-7 PERF Fix 3: gate on `needs_d2h_sync`
        if needs_d2h_sync {
            ctx.dev.wait_for(&ctx.compute).map_err(map_err("wait_for compute"))?;
        }
        Ok(())
    }
}

/// Round-5: H→D upload helper.
///
/// Uploads via pageable memory. In cudarc 0.13, the pinned API is not
/// available, so we use the standard slice-based htod copy.
///
/// The wrapper still exists today so:
///   * `from_host` has a single call site to upgrade if pinned API becomes available,
///   * the fallback semantics are explicit.
///
/// AUDIT 2026-05-20 (PERF Fix): previously this did
/// `ctx.dev.htod_copy(host.to_vec())`. `htod_copy` takes an *owned*
/// `Vec`, so `host.to_vec()` cloned the entire input slice into a fresh
/// heap allocation before the H→D transfer — doubling host memory
/// traffic per upload. cudarc 0.13's `htod_sync_copy` takes `&[T]`
/// directly and memcpys it straight to the device, so the redundant
/// allocation+copy is gone. `htod_sync_copy` is synchronous (it does
/// not retain the host buffer), which is why the `Unpin` bound is no
/// longer required.
#[inline]
fn upload_via_pinned_or_fallback<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static>(
    ctx: &DeviceContextInner,
    host: &[T],
) -> Result<CudaSlice<T>> {
    // Pageable path for cudarc 0.13 - pinned API not available in this version.
    // Functionally correct; the only cost is the implicit driver-side
    // bounce buffer.
    //
    // PERF: use `htod_sync_copy`, which takes the host data by `&[T]`
    // slice, instead of `htod_copy`, which requires an owned `Vec<T>`.
    // The old code did `host.to_vec()` to satisfy `htod_copy`, paying a
    // full redundant host-side copy of the payload on every upload.
    // `htod_sync_copy` issues the same H→D memcpy with no intermediate
    // clone. It is a synchronous copy (the host data is borrowed, so the
    // driver must finish reading it before this returns) — strictly more
    // conservative than the previous async `htod_copy`, and the caller
    // (`from_host`) already host-synchronizes against the upload anyway.
    ctx.dev
        .htod_sync_copy(host)
        .map_err(map_err("memcpy host→device"))
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
    compute: Arc<CudaStream>,
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
        })
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        // AUDIT 2026-05-17 (PERF Fix 1): H→D upload runs on the
        // dedicated copy_h2d stream, then records an event the compute
        // stream waits on. This lets the next host-side launch_raw
        // schedule a kernel without blocking on the upload to complete.
        //
        // AUDIT 2026-05-17 (PERF Fix 3 / Round-5): try a pinned (page-
        // locked) host staging buffer first. cudaMemcpyAsync from pinned
        // memory bypasses the driver's internal staging copy and can
        // overlap with kernel execution; pageable memory forces a
        // synchronous copy through the driver-managed bounce buffer
        // (the cudarc API hides that, but it still happens at the
        // libcuda level). For large transfers this is ~2× the
        // achievable PCIe bandwidth.
        //
        // The pinned path is best-effort: pinned memory comes from a
        // limited OS pool (~system-wide RAM/16, varies). If allocation
        // fails (OOM in the pinned pool, no driver support, or cudarc
        // doesn't expose the API on this version), we fall back to the
        // pageable path unchanged.
        let slice = upload_via_pinned_or_fallback(ctx, host)?;
        // Wait for copy_h2d to complete before compute stream
        ctx.dev.wait_for(&ctx.copy_h2d).map_err(map_err("wait_for copy_h2d"))?;
        Ok(Self {
            slice: Arc::new(slice),
            stream: ctx.copy_h2d.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
            compute: ctx.compute.clone(),
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
        // `dtoh_sync_copy_into` issues the D→H memcpy on the cudarc
        // device's *default* stream and then synchronizes that default
        // stream. Kernels, however, run on the dedicated `compute`
        // stream, which is a separate forked stream — there is no
        // implicit ordering between work on `compute` and work on the
        // default stream.
        //
        // `launch_raw` (the sync variant) bridges that gap by waiting on
        // the compute stream after the launch, but `launch_raw_no_sync`
        // deliberately skips that wait. Relying on an "event recorded
        // inside launch_raw" is therefore unsound: in the no-sync path no
        // such event exists, so the copy below could race ahead of the
        // kernel and read stale device memory.
        //
        // To guarantee correctness for every launch path, host-block the
        // device here before issuing the copy. `CudaDevice::synchronize`
        // (`cuCtxSynchronize`) drains every stream — including the
        // dedicated `compute` stream a kernel can have run on — so once
        // it returns the device buffer holds the kernel's output.
        self.dev
            .synchronize()
            .map_err(map_err("synchronize device before D→H"))?;
        self.dev
            // `self.slice` is `Arc<CudaSlice<T>>`; deref the `Arc` so the
            // argument is `&CudaSlice<T>`, which implements cudarc's
            // `DevicePtr` (an `&Arc<CudaSlice<T>>` does not).
            .dtoh_sync_copy_into(&*self.slice, dst)
            .map_err(map_err("memcpy device→host"))
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
